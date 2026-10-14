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
use std::io::{BufRead as _, Read as _, Write as _};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Clone)]
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
    ModelList {
        pane: Uuid,
        forge_sid: Uuid,
        req_id: String,
    },
    /// Switch the session's model (PATCH /sessions/:id model switcher).
    ModelSet {
        pane: Uuid,
        forge_sid: Uuid,
        req_id: String,
        provider: String,
        model: String,
    },
    /// Fetch the session's context-window usage (GET /sessions/:id/context)
    /// and broadcast it as `meta { kind: "context" }`.
    #[allow(dead_code)]
    Context { pane: Uuid, forge_sid: Uuid },
    /// Manually compact the session's pi context (POST /sessions/:id/compact).
    /// Success → a `meta { kind: "context" }` with the new usage (plus a
    /// refresh); failure → `error { req_id }`.
    Compact {
        pane: Uuid,
        forge_sid: Uuid,
        req_id: String,
    },
    /// Interrupt the in-flight turn (POST /sessions/:id/interrupt).
    /// Immediate and non-destructive; the forge system row + idle status
    /// ride back on the SSE watch, so no reply frame is needed.
    Interrupt { forge_sid: Uuid },
    // ----- agent builder (Phase B): profile CRUD proxy -----
    /// The pi model catalog (GET /v1/models/catalog) for profile forms.
    ModelCatalog { req_id: String },
    /// List agent profiles (GET /profiles).
    ProfileList { req_id: String },
    /// Fetch one profile (GET /profiles/{id}; api_key arrives redacted).
    ProfileGet { req_id: String, profile_id: String },
    /// Create (no id) or update (id) a profile. Payload is boxed: the
    /// draft is ~12 strings and would bloat the enum.
    ProfilePut(Box<ProfilePutBody>),
    /// Delete a profile (DELETE /profiles/{id}).
    ProfileDelete { req_id: String, profile_id: String },
    // ----- agent registry (Herd H1) -----
    /// List the agents registered in forge (GET /agents).
    AgentList { req_id: String },
    /// Is this agent busy right now? (GET /agents/{id}/active).
    AgentBusy { req_id: String, agent: String },
}

/// Payload for `ForgeJob::ProfilePut` (boxed out of the enum to keep it
/// small — ProfileDraft carries a dozen strings).
pub struct ProfilePutBody {
    pub req_id: String,
    pub profile_id: Option<String>,
    pub draft: ranch_protocol::ProfileDraft,
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
    /// true when the current turn was initiated by a mule wake —
    /// the idle Meta carries `background: true` so clients suppress
    /// the turn-end notification
    background_turn: AtomicBool,
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
    let tool_args = r.get("tool_input").and_then(|t| {
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
    // tool rows whose input references an image become viewable: set
    // image_refs + register the path so clients can FileGet it
    let image_refs = tool_args.as_ref().and_then(|a| {
        let refs = crate::daemon::media::image_refs_from_tool_args(a);
        if refs.is_empty() {
            None
        } else {
            refs.iter().for_each(|r| crate::daemon::media::register(r));
            Some(refs)
        }
    });
    Some(ChatMsg {
        seq,
        role,
        text,
        tool_name,
        tool_call_id,
        tool_output,
        tool_args,
        duration_ms,
        created_at,
        attachments: None,
        image_refs,
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
                // mule-wake-initiated turns are background: the user
                // should not get a turn-end notification unless the
                // agent explicitly calls notify_user
                let is_bg = msg.text.starts_with("[mule wake");
                state.background_turn.store(is_bg, Ordering::Relaxed);
                write_agent_status(w, state.pane, "working");
            }
            write_frame(
                w,
                &Frame::Chat {
                    id: String::new(),
                    session: String::new(), // filled by the main loop
                    pane: state.pane.to_string(),
                    msgs: vec![msg.clone()],
                    reset: false,
                },
            );
            // @notify: prefix = explicit user notification (the agent
            // called notify_user, which POSTs the prefixed text via
            // POST /messages — so it lands as a USER row; assistant
            // rows are covered too, e.g. a mule step that writes it).
            if (msg.role == "user" || msg.role == "assistant") && msg.text.starts_with("@notify:") {
                let body = msg.text.trim_start_matches("@notify:").trim().to_string();
                let pane = state.pane;
                let sid = state.forge_sid;
                let cfg = cfg.clone();
                let w = w.clone();
                std::thread::spawn(move || {
                    let agent = notify_agent_name(&cfg, sid);
                    write_frame(
                        &w,
                        &Frame::AgentNotify {
                            session: String::new(),
                            pane: pane.to_string(),
                            agent,
                            message: body,
                            priority: None,
                        },
                    );
                });
            }
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
            let cfg = cfg.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(2500));
                if state.stop.load(Ordering::Relaxed)
                    || state.turn_gen.load(Ordering::Relaxed) != before
                {
                    return; // cancelled: a new turn started
                }
                let bg = state.background_turn.swap(false, Ordering::Relaxed);
                write_agent_status_bg(&w, pane, "idle", bg);
                // the turn may have grown the context — refresh the
                // readout so clients show the new usage
                emit_context(&w, &cfg, pane, state.forge_sid);
            });
        }
        "ranch_tool_request" => {
            // forge-side bridge (ranch PLAN F3a): a remote forge agent
            // called a ranch_* tool; execute it via the loopback control
            // API (this process IS the daemon — the env vars are ours)
            // and POST the result back to forge. Runs on a thread: the
            // SSE reader must not block on the call.
            let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
                return;
            };
            let id = v
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let tool = v
                .get("tool")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let input = v.get("input").cloned().unwrap_or(serde_json::Value::Null);
            let cfg = cfg.clone();
            let pane = state.pane;
            if tool == "policy_ask" {
                // Herd H3.5: mule's policy engine returned an `ask`
                // verdict — the approval round-trip is a ranch
                // AgentAsk (broadcast to every client), relayed back to
                // forge as the policy-ask decision.
                std::thread::spawn(move || {
                    let result = execute_policy_ask_via_ctl(&input, pane);
                    relay_result_to_forge(&cfg, &id, &result);
                });
                return;
            }
            if tool == "web_login" {
                // Herd H6.5: the web sign-in broker — the approval
                // card ("Sign me in" / "Cancel"). On approval the
                // result door mints the one-time handoff token and
                // opens the USER's browser; the cookie never passes
                // through this lane (the input is {url, slot} only).
                std::thread::spawn(move || {
                    let result = execute_web_login_via_ctl(&input, pane);
                    relay_result_to_forge(&cfg, &id, &result);
                });
                return;
            }
            if tool == "memory_review" {
                // Herd H4.4: forge proposed a pending belief and pushed
                // the approval card — same AgentAsk round-trip shape as
                // policy_ask, but with Keep/Forget choices plus free
                // text (the text IS the Edit answer). The answer
                // payload rides the result relay; forge applies the
                // belief transition on the result door.
                std::thread::spawn(move || {
                    let result = execute_memory_review_via_ctl(&input, pane);
                    relay_card_result_to_forge(&cfg, &id, &result);
                });
                return;
            }
            if tool == "research_report" {
                // Herd H5.1: a research task settled and forge pushed
                // the suggestion card — Use / Discard / Ask more via
                // the same AgentAsk round-trip; the answer payload
                // (`{action: use|discard|ask_more, followup}`) rides
                // the result relay and forge resolves the
                // agent_research row on the result door.
                std::thread::spawn(move || {
                    let result = execute_research_report_via_ctl(&input, pane);
                    relay_card_result_to_forge(&cfg, &id, &result);
                });
                return;
            }
            if id.is_empty() || !tool.starts_with("ranch_") {
                return;
            }
            std::thread::spawn(move || {
                let result = execute_ranch_tool_via_ctl(&tool, &input, pane);
                relay_result_to_forge(&cfg, &id, &result);
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

/// Execute one `ranch_*` tool through the loopback control API — the
/// same door local-pi agents use, so ownership/policy semantics are
/// identical. `caller_pane` is the forge agent's own pane.
fn execute_ranch_tool_via_ctl(
    tool: &str,
    input: &serde_json::Value,
    caller_pane: Uuid,
) -> serde_json::Value {
    let route = tool.strip_prefix("ranch_").unwrap_or(tool);
    let port = std::env::var("RANCH_CONTROL_PORT").unwrap_or_default();
    let token = std::env::var("RANCH_CONTROL_TOKEN").unwrap_or_default();
    if port.is_empty() || token.is_empty() {
        return serde_json::json!({
            "success": false,
            "error": "ranch control API unavailable (no RANCH_CONTROL_PORT)"
        });
    }
    let body = serde_json::json!({
        "caller_pane": caller_pane.to_string(),
        // flatten the tool's JSON params into the control body
        "pane": input.get("pane").and_then(|x| x.as_str()).unwrap_or(""),
        "text": input.get("text").and_then(|x| x.as_str()).unwrap_or(""),
        "delivery": input.get("delivery").and_then(|x| x.as_str()).unwrap_or("steer"),
        "since_seq": input.get("since_seq").and_then(|x| x.as_i64()).unwrap_or(0),
        "limit": input.get("limit").and_then(|x| x.as_u64()).unwrap_or(50),
    });
    let url = format!("http://127.0.0.1:{port}/agent/{route}");
    match ureq::post(&url)
        .header("Authorization", &format!("Bearer {token}"))
        .header("X-Ranch-Pane", &caller_pane.to_string())
        .send_json(body)
    {
        Ok(mut res) => {
            let mut text = String::new();
            use std::io::Read as _;
            let _ = res.body_mut().as_reader().read_to_string(&mut text);
            serde_json::from_str(&text).unwrap_or(serde_json::json!({ "raw": text }))
        }
        Err(e) => {
            serde_json::json!({ "success": false, "error": format!("control call failed: {e}") })
        }
    }
}

/// Shared AgentAsk round-trip against the loopback control API (the
/// H3.5 policy lane + the H6.5 web_login lane): POST /agent/ask, then
/// poll /agent/ask-status (~2 s cadence) until answered or the ~55 s
/// deadline — just under forge's 60 s relay TTL, so a slow human
/// surfaces as forge's `expired` (the harness fails closed) rather
/// than as our socket timing out under forge's. Choice 0 is the
/// affirmative mapping ("Allow this action" / "Sign me in");
/// anything else — or the deadline — maps to success:false.
fn agent_ask_round_trip(
    caller_pane: Uuid,
    question: &str,
    choices: &[&str],
) -> (bool, Option<String>) {
    let port = std::env::var("RANCH_CONTROL_PORT").unwrap_or_default();
    let token = std::env::var("RANCH_CONTROL_TOKEN").unwrap_or_default();
    if port.is_empty() || token.is_empty() {
        return (
            false,
            Some("ranch control API unavailable (no RANCH_CONTROL_PORT)".into()),
        );
    }
    let ask_body = serde_json::json!({
        "caller_pane": caller_pane.to_string(),
        "question": question,
        "choices": choices,
        "free_text": false,
    });
    let frames = match ctl_post_frames(&port, &token, caller_pane, "ask", &ask_body) {
        Ok(f) => f,
        Err(e) => return (false, Some(format!("control API error: {e}"))),
    };
    let ask_id = frames
        .iter()
        .find(|f| f.get("t").and_then(|x| x.as_str()) == Some("AgentAskOk"))
        .and_then(|f| f.get("ask_id").and_then(|x| x.as_str()))
        .unwrap_or("")
        .to_string();
    if ask_id.is_empty() {
        return (false, Some("no AgentAskOk in the control reply".into()));
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(55);
    loop {
        let frames = match ctl_post_frames(
            &port,
            &token,
            caller_pane,
            "ask-status",
            &serde_json::json!({ "caller_pane": caller_pane.to_string(), "ask_id": ask_id }),
        ) {
            Ok(f) => f,
            Err(e) => return (false, Some(format!("status poll failed: {e}"))),
        };
        if let Some(st) = frames
            .iter()
            .find(|f| f.get("t").and_then(|x| x.as_str()) == Some("AgentAskStatusOk"))
        {
            let state = st
                .get("state")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown");
            match state {
                "answered" => {
                    let choices = st
                        .get("choices")
                        .and_then(|c| c.as_array())
                        .cloned()
                        .unwrap_or_default();
                    return if choices.iter().any(|c| c.as_i64() == Some(0)) {
                        (true, None)
                    } else {
                        (false, Some("user denied".into()))
                    };
                }
                "unknown" => return (false, Some("no answer in time".into())),
                _ => {} // "pending" — keep polling
            }
        }
        if std::time::Instant::now() >= deadline {
            return (false, Some("no answer in time".into()));
        }
        std::thread::sleep(std::time::Duration::from_millis(2000));
    }
}

/// Herd H6.5: the web sign-in broker's approval card. The question
/// carries `{url, slot}` ONLY — the sign-in happens in the user's own
/// browser (password typed by the user, cookie pasted into forge's
/// handoff page), so no credential material rides this lane. "Sign me
/// in" (choice 0) → success (forge mints the one-time handoff token +
/// xdg-opens the handoff page); "Cancel" → forge reports `denied`.
fn execute_web_login_via_ctl(input: &serde_json::Value, caller_pane: Uuid) -> serde_json::Value {
    let url = input.get("url").and_then(|x| x.as_str()).unwrap_or("");
    let slot = input.get("slot").and_then(|x| x.as_str()).unwrap_or("");
    let question = format!(
        "Agent wants to sign in to {url} so its sandbox can use the '{slot}' credential.\n\
         The sign-in happens in YOUR browser: you type the password yourself, the agent never sees it.\n\
         Sign me in?"
    );
    let (success, error) = agent_ask_round_trip(caller_pane, &question, &["Sign me in", "Cancel"]);
    if success {
        serde_json::json!({ "success": true })
    } else {
        serde_json::json!({
            "success": false,
            "error": format!("web_login: {}", error.unwrap_or_default()),
        })
    }
}

/// Herd H3.5: drive the ranch approval round-trip for a mule policy
/// `ask` verdict (the forge-side `POST /sessions/{id}/policy-ask`
/// long-poll resolves on our result POST). Posts an `AgentAsk` to the
/// loopback control API — the daemon broadcasts `AgentAskRequest` to
/// every client, which render it with their EXISTING ask-answer
/// plumbing (TUI status bar, web/mobile card, mobile notification):
/// no protocol change, the rule context rides in the `question` text.
fn execute_policy_ask_via_ctl(input: &serde_json::Value, caller_pane: Uuid) -> serde_json::Value {
    let rule_id = input.get("rule_id").and_then(|x| x.as_str()).unwrap_or("");
    let reason = input.get("reason").and_then(|x| x.as_str()).unwrap_or("");
    let tool = input.get("tool").and_then(|x| x.as_str()).unwrap_or("");
    let tool_input = input
        .get("input")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let input_brief = truncate_str(&serde_json::to_string(&tool_input).unwrap_or_default(), 500);
    let question = format!(
        "Agent wants to run `{tool}` {input_brief}.\nOperator policy rule '{rule_id}': {reason}\nAllow this action?"
    );
    let (success, error) =
        agent_ask_round_trip(caller_pane, &question, &["Allow this action", "Deny"]);
    if success {
        serde_json::json!({ "success": true })
    } else {
        serde_json::json!({
            "success": false,
            "error": format!("policy ask: {}", error.unwrap_or_default()),
        })
    }
}

/// Herd H4.4: drive the ranch approval round-trip for a forge belief
/// proposal (the `memory_review` card). Same AgentAsk plumbing as the
/// H3.5 policy lane, but the card offers **Keep / Forget plus free
/// text** — a free-text answer IS the Edit: the typed text becomes
/// the belief's new content. Polls `AgentAskStatus` (~2 s cadence)
/// until answered or the ~55 s deadline, just under forge's 60 s
/// queue TTL: a slow human means the belief simply STAYS PENDING
/// (v1 does not re-push the card; it remains visible via
/// `GET /memory/beliefs?status=pending` and answerable via the
/// keep/forget routes). The choice mapping is [`memory_review_reply`]
/// (unit-tested); this function only drives the control API.
fn execute_memory_review_via_ctl(
    input: &serde_json::Value,
    caller_pane: Uuid,
) -> serde_json::Value {
    let port = std::env::var("RANCH_CONTROL_PORT").unwrap_or_default();
    let token = std::env::var("RANCH_CONTROL_TOKEN").unwrap_or_default();
    if port.is_empty() || token.is_empty() {
        return serde_json::json!({
            "success": false,
            "error": "ranch control API unavailable (no RANCH_CONTROL_PORT)"
        });
    }
    let content = input.get("content").and_then(|x| x.as_str()).unwrap_or("");
    let rationale = input
        .get("rationale")
        .and_then(|x| x.as_str())
        .unwrap_or("");
    let question = if rationale.is_empty() {
        format!("I've learned: **{content}** — keep this memory?")
    } else {
        format!("I've learned: **{content}** ({rationale}) — keep this memory?")
    };

    // 1 — ask: POST /agent/ask → AgentAskOk { ask_id }. Free text next
    // to the Keep/Forget choices carries the Edit answer.
    let ask_body = serde_json::json!({
        "caller_pane": caller_pane.to_string(),
        "question": question,
        "choices": ["Keep", "Forget"],
        "suggested": 0,
        "free_text": true,
    });
    let frames = match ctl_post_frames(&port, &token, caller_pane, "ask", &ask_body) {
        Ok(f) => f,
        Err(e) => {
            return serde_json::json!({ "success": false, "error": format!("memory review: control API error: {e}") });
        }
    };
    let ask_id = frames
        .iter()
        .find(|f| f.get("t").and_then(|x| x.as_str()) == Some("AgentAskOk"))
        .and_then(|f| f.get("ask_id").and_then(|x| x.as_str()))
        .unwrap_or("")
        .to_string();
    if ask_id.is_empty() {
        return serde_json::json!({ "success": false, "error": "memory review: no AgentAskOk in the control reply" });
    }

    // 2 — poll: /agent/ask-status every ~2 s until answered or ~55 s.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(55);
    loop {
        let frames = match ctl_post_frames(
            &port,
            &token,
            caller_pane,
            "ask-status",
            &serde_json::json!({ "caller_pane": caller_pane.to_string(), "ask_id": ask_id }),
        ) {
            Ok(f) => f,
            Err(e) => {
                return serde_json::json!({ "success": false, "error": format!("memory review: status poll failed: {e}") });
            }
        };
        if let Some(st) = frames
            .iter()
            .find(|f| f.get("t").and_then(|x| x.as_str()) == Some("AgentAskStatusOk"))
        {
            let state = st
                .get("state")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown");
            if state == "answered" {
                let choices: Vec<i64> = st
                    .get("choices")
                    .and_then(|c| c.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
                    .unwrap_or_default();
                let text = st.get("text").and_then(|x| x.as_str()).unwrap_or("");
                return memory_review_reply(&choices, text);
            }
            if state == "unknown" {
                return serde_json::json!({ "success": false, "error": "memory review: no answer in time" });
            }
            // "pending" — keep polling
        }
        if std::time::Instant::now() >= deadline {
            return serde_json::json!({ "success": false, "error": "memory review: no answer in time" });
        }
        std::thread::sleep(std::time::Duration::from_millis(2000));
    }
}

/// Map a memory_review card answer (selected choice indices + free
/// text) to the result payload forge applies on the belief. Pure,
/// so the contract is unit-testable:
///
/// - free text (non-blank) → `edit`: the text becomes the belief's
///   new content (takes precedence over any choice — the user typed a
///   correction, that's the intent);
/// - choice 0 ("Keep") → `keep`;
/// - any other selection (choice 1, "Forget") → `forget`;
/// - nothing selected and nothing typed → a failed relay (the belief
///   stays pending).
fn memory_review_reply(choices: &[i64], text: &str) -> serde_json::Value {
    let text = text.trim();
    if !text.is_empty() {
        return serde_json::json!({ "success": true, "output": { "action": "edit", "content": text } });
    }
    if choices.is_empty() {
        return serde_json::json!({ "success": false, "error": "memory review: no answer in time" });
    }
    if choices.contains(&0) {
        serde_json::json!({ "success": true, "output": { "action": "keep" } })
    } else {
        serde_json::json!({ "success": true, "output": { "action": "forget" } })
    }
}

/// Herd H5.1: drive the ranch approval round-trip for a settled
/// research task (the `research_report` card). Same AgentAsk
/// plumbing as the H4.4 memory lane: Use / Discard / Ask more
/// choices, with free text carrying the Ask-more follow-up. Polls
/// `AgentAskStatus` (~2 s cadence) until answered or the ~55 s
/// deadline — just under forge's 60 s queue TTL, so a slow human
/// means the row STAYS `done` (still open, answerable later via
/// `GET /agents/:id/research?open=1`; forge's card handler treats a
/// failed relay as "no answer"). The choice mapping is
/// [`research_report_reply`] (unit-tested); this function only
/// drives the control API.
fn execute_research_report_via_ctl(
    input: &serde_json::Value,
    caller_pane: Uuid,
) -> serde_json::Value {
    let port = std::env::var("RANCH_CONTROL_PORT").unwrap_or_default();
    let token = std::env::var("RANCH_CONTROL_TOKEN").unwrap_or_default();
    if port.is_empty() || token.is_empty() {
        return serde_json::json!({
            "success": false,
            "error": "ranch control API unavailable (no RANCH_CONTROL_PORT)"
        });
    }
    let question = input.get("question").and_then(|x| x.as_str()).unwrap_or("");
    let summary = input.get("summary").and_then(|x| x.as_str()).unwrap_or("");
    let ask = if summary.is_empty() {
        format!("Research done: **{question}** — use this, discard it, or ask for more?")
    } else {
        format!("**{question}**\n\n{summary}\n\nUse this, discard it, or ask for more?")
    };

    // 1 — ask: POST /agent/ask → AgentAskOk { ask_id }. Free text next
    // to the Use/Discard/Ask-more choices carries the follow-up.
    let ask_body = serde_json::json!({
        "caller_pane": caller_pane.to_string(),
        "question": ask,
        "choices": ["Use", "Discard", "Ask more"],
        "suggested": 0,
        "free_text": true,
    });
    let frames = match ctl_post_frames(&port, &token, caller_pane, "ask", &ask_body) {
        Ok(f) => f,
        Err(e) => {
            return serde_json::json!({ "success": false, "error": format!("research report: control API error: {e}") });
        }
    };
    let ask_id = frames
        .iter()
        .find(|f| f.get("t").and_then(|x| x.as_str()) == Some("AgentAskOk"))
        .and_then(|f| f.get("ask_id").and_then(|x| x.as_str()))
        .unwrap_or("")
        .to_string();
    if ask_id.is_empty() {
        return serde_json::json!({ "success": false, "error": "research report: no AgentAskOk in the control reply" });
    }

    // 2 — poll: /agent/ask-status every ~2 s until answered or ~55 s.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(55);
    loop {
        let frames = match ctl_post_frames(
            &port,
            &token,
            caller_pane,
            "ask-status",
            &serde_json::json!({ "caller_pane": caller_pane.to_string(), "ask_id": ask_id }),
        ) {
            Ok(f) => f,
            Err(e) => {
                return serde_json::json!({ "success": false, "error": format!("research report: status poll failed: {e}") });
            }
        };
        if let Some(st) = frames
            .iter()
            .find(|f| f.get("t").and_then(|x| x.as_str()) == Some("AgentAskStatusOk"))
        {
            let state = st
                .get("state")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown");
            if state == "answered" {
                let choices: Vec<i64> = st
                    .get("choices")
                    .and_then(|c| c.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
                    .unwrap_or_default();
                let text = st.get("text").and_then(|x| x.as_str()).unwrap_or("");
                return research_report_reply(&choices, text);
            }
            if state == "unknown" {
                return serde_json::json!({ "success": false, "error": "research report: no answer in time" });
            }
            // "pending" — keep polling
        }
        if std::time::Instant::now() >= deadline {
            return serde_json::json!({ "success": false, "error": "research report: no answer in time" });
        }
        std::thread::sleep(std::time::Duration::from_millis(2000));
    }
}

/// Map a research_report card answer (selected choice indices + free
/// text) to the result payload forge applies on the agent_research
/// row. Pure, so the contract is unit-testable:
///
/// - choice 2 ("Ask more") with non-blank text → `ask_more` +
///   `followup`: the text is re-injected into the research
///   conversation and the task runs again;
/// - free text WITHOUT any choice → `ask_more` (a bare typed reply
///   means "dig further");
/// - choice 0 ("Use") → `use` (the finding is adopted — the
///   row leaves the open listing);
/// - choice 1 ("Discard") → `discard`;
/// - "Ask more" with blank text, or nothing at all → a failed relay
///   (forge's handler treats that as "no answer"; the row stays
///   `done`, still open).
fn research_report_reply(choices: &[i64], text: &str) -> serde_json::Value {
    let text = text.trim();
    if choices.contains(&2) && !text.is_empty() {
        return serde_json::json!({ "success": true, "output": { "action": "ask_more", "followup": text } });
    }
    if choices.is_empty() && !text.is_empty() {
        return serde_json::json!({ "success": true, "output": { "action": "ask_more", "followup": text } });
    }
    if choices.contains(&0) {
        return serde_json::json!({ "success": true, "output": { "action": "use" } });
    }
    if choices.contains(&1) {
        return serde_json::json!({ "success": true, "output": { "action": "discard" } });
    }
    serde_json::json!({ "success": false, "error": "research report: no answer in time" })
}

/// One loopback control-API POST (`/agent/<route>`): the control API
/// answers with a JSON array of frames; return it.
fn ctl_post_frames(
    port: &str,
    token: &str,
    caller_pane: Uuid,
    route: &str,
    body: &serde_json::Value,
) -> Result<Vec<serde_json::Value>, String> {
    let url = format!("http://127.0.0.1:{port}/agent/{route}");
    let mut res = ureq::post(&url)
        .header("Authorization", &format!("Bearer {token}"))
        .header("X-Ranch-Pane", &caller_pane.to_string())
        .send_json(body)
        .map_err(|e| e.to_string())?;
    let mut text = String::new();
    res.body_mut()
        .as_reader()
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// Trim a string to `max` chars (on a char boundary), ellipsis-marked.
fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

/// Translate a control-API reply into the relay result shape forge
/// expects and POST it back (F3a result endpoint).
fn relay_result_to_forge(cfg: &ForgeConfig, id: &str, reply: &serde_json::Value) {
    // error replies ({error: ...}) become failed tool results; ok
    // replies (AgentStatusOk/AgentReadOk/...) become success output
    let (success, output, error) = if let Some(err) = reply.get("error").and_then(|x| x.as_str()) {
        (false, serde_json::Value::Null, Some(err.to_string()))
    } else {
        (true, reply.clone(), None)
    };
    post_result_to_forge(cfg, id, success, output, error);
}

/// Relay a CARD-lane answer (H4.4 `memory_review`, H5.1
/// `research_report`): the executor's reply is
/// `{success, output: <the answer>, error}` and forge's apply handler
/// reads the answer at the TOP LEVEL of the result body's `output`
/// (`output.action`), so the relay flattens the inner `output` down —
/// posting the whole reply would nest it one level too deep and
/// forge would see no `action`.
fn relay_card_result_to_forge(cfg: &ForgeConfig, id: &str, reply: &serde_json::Value) {
    let (success, output, error) = if reply.get("success").and_then(|x| x.as_bool()) == Some(true) {
        (
            true,
            reply
                .get("output")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            None,
        )
    } else {
        (
            false,
            serde_json::Value::Null,
            reply
                .get("error")
                .and_then(|x| x.as_str())
                .map(str::to_string),
        )
    };
    post_result_to_forge(cfg, id, success, output, error);
}

/// POST one `{success, output, error}` body to forge's
/// `/ranch-tools/:id/result` door.
fn post_result_to_forge(
    cfg: &ForgeConfig,
    id: &str,
    success: bool,
    output: serde_json::Value,
    error: Option<String>,
) {
    let body = serde_json::json!({ "success": success, "output": output, "error": error });
    let url = format!("{}/ranch-tools/{}/result", cfg.base, id);
    let res = ureq::post(&url)
        .header("X-API-Key", &cfg.key)
        .send_json(body);
    match res {
        Ok(_) => {}
        Err(e) => eprintln!("forge-sse: ranch tool result relay failed: {e}"),
    }
}

/// Agent busy/idle signal (client typing indicator).
fn write_agent_status(w: &PipeWriter, pane: Uuid, status: &str) {
    write_agent_status_bg(w, pane, status, false);
}

/// Like write_agent_status but carries the `background` flag on the
/// Meta frame (used for the idle transition that closes a mule-wake
/// turn, so mobile clients can suppress the turn-end notification).
fn write_agent_status_bg(w: &PipeWriter, pane: Uuid, status: &str, background: bool) {
    write_frame(
        w,
        &Frame::Meta {
            session: String::new(), // filled by the main loop
            pane: Some(pane.to_string()),
            kind: "agent".into(),
            status: Some(status.into()),
            background: if background { Some(true) } else { None },
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
                            background_turn: AtomicBool::new(false),
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
                                background: None,
                                },
                            );
                        }
                        // initial context-window readout
                        let t_pipe2 = pipe.clone();
                        let t_cfg2 = t_cfg.clone();
                        std::thread::spawn(move || {
                            emit_context(&t_pipe2, &t_cfg2, pane, forge_sid);
                        });
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
                                    let working_dir = s
                                        .get("working_dir")
                                        .and_then(|t| t.as_str())
                                        .map(|t| t.to_string());
                                    Some(ranch_protocol::ForgeSessionInfo {
                                        id,
                                        title,
                                        updated,
                                        ended,
                                        working_dir,
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
                    ForgeJob::Context { pane, forge_sid } => {
                        emit_context(&pipe, &cfg, pane, forge_sid);
                    }
                    ForgeJob::Compact {
                        pane,
                        forge_sid,
                        req_id,
                    } => {
                        let path = format!("/sessions/{forge_sid}/compact");
                        match http_json(&cfg, "POST", &path, Some(&serde_json::json!({}))) {
                            Ok(v) => {
                                let after = v
                                    .get("estimated_tokens_after")
                                    .and_then(|a| a.as_i64())
                                    .map(format_k)
                                    .unwrap_or_default();
                                let note = if after.is_empty() {
                                    "compacted".to_string()
                                } else {
                                    format!("compacted → {after} tokens")
                                };
                                write_frame(
                                    &pipe,
                                    &Frame::Meta {
                                        session: String::new(),
                                        pane: Some(pane.to_string()),
                                        kind: "context".into(),
                                        status: Some(note),
                                    background: None,
                                    },
                                );
                                // close the "compacting" lifecycle machine-wide
                                write_frame(
                                    &pipe,
                                    &Frame::Meta {
                                        session: String::new(),
                                        pane: Some(pane.to_string()),
                                        kind: "agent".into(),
                                        status: Some("idle".into()),
                                    background: None,
                                    },
                                );
                                // the compaction changed the window — refresh
                                let t_pipe = pipe.clone();
                                let t_cfg = cfg.clone();
                                std::thread::spawn(move || {
                                    emit_context(&t_pipe, &t_cfg, pane, forge_sid);
                                });
                            }
                            Err(e) => {
                                eprintln!("ranchd: forge compact failed: {e}");
                                write_frame(
                                    &pipe,
                                    &Frame::Error {
                                        req_id: Some(req_id),
                                        message: format!("forge compact: {e}"),
                                    },
                                );
                            }
                        }
                    }
                    ForgeJob::Interrupt { forge_sid } => {
                        let path = format!("/sessions/{forge_sid}/interrupt");
                        match http_json(&cfg, "POST", &path, Some(&serde_json::json!({}))) {
                            Ok(_) => {
                                // forge records a system row and the turn
                                // ends; the SSE watch delivers the row +
                                // idle status, so nothing to emit here.
                            }
                            Err(e) => {
                                eprintln!("ranchd: forge interrupt failed: {e}");
                            }
                        }
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
                                background: None,
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
                    // ----- agent builder (Phase B): profile CRUD proxy -----
                    ForgeJob::ModelCatalog { req_id } => {
                        let (models, _) = fetch_catalog(&cfg);
                        write_frame(&pipe, &Frame::ModelCatalogOk { req_id, models });
                    }
                    ForgeJob::ProfileList { req_id } => {
                        match http_json(&cfg, "GET", "/profiles", None) {
                            Ok(v) => {
                                let arr = v
                                    .get("profiles")
                                    .and_then(|x| x.as_array())
                                    .cloned()
                                    .or_else(|| v.as_array().cloned())
                                    .unwrap_or_default();
                                let profiles: Vec<ranch_protocol::ProfileSummary> = arr
                                    .iter()
                                    .filter_map(|p| {
                                        Some(ranch_protocol::ProfileSummary {
                                            id: p.get("id")?.as_str()?.to_string(),
                                            name: p
                                                .get("name")
                                                .and_then(|n| n.as_str())
                                                .unwrap_or("")
                                                .into(),
                                            description: p
                                                .get("description")
                                                .and_then(|d| d.as_str())
                                                .map(String::from),
                                            provider: p
                                                .get("provider")
                                                .and_then(|n| n.as_str())
                                                .unwrap_or("")
                                                .into(),
                                            model: p
                                                .get("model")
                                                .and_then(|n| n.as_str())
                                                .unwrap_or("")
                                                .into(),
                                            working_dir: p
                                                .get("working_dir")
                                                .and_then(|d| d.as_str())
                                                .map(String::from),
                                            updated_at: p
                                                .get("updated_at")
                                                .and_then(|d| d.as_str())
                                                .map(String::from),
                                        })
                                    })
                                    .collect();
                                write_frame(&pipe, &Frame::ProfileListOk { req_id, profiles });
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
                    ForgeJob::ProfileGet { req_id, profile_id } => {
                        match http_json(&cfg, "GET", &format!("/profiles/{profile_id}"), None) {
                            Ok(v) => match serde_json::from_value::<ranch_protocol::Profile>(v) {
                                Ok(prof) => {
                                    write_frame(
                                        &pipe,
                                        &Frame::ProfileGetOk {
                                            req_id,
                                            profile: prof,
                                        },
                                    );
                                }
                                Err(e) => write_frame(
                                    &pipe,
                                    &Frame::Error {
                                        req_id: Some(req_id),
                                        message: format!("profile decode: {e}"),
                                    },
                                ),
                            },
                            Err(e) => write_frame(
                                &pipe,
                                &Frame::Error {
                                    req_id: Some(req_id),
                                    message: e,
                                },
                            ),
                        }
                    }
                    ForgeJob::ProfilePut(body) => {
                        let ProfilePutBody {
                            req_id,
                            profile_id,
                            draft,
                        } = *body;

                        let body = serde_json::json!({
                            "name": draft.name,
                            "description": draft.description,
                            "provider": draft.provider,
                            "model": draft.model,
                            "base_url": draft.base_url,
                            "api_key": draft.api_key,
                            "working_dir": draft.working_dir,
                            "git_url": draft.git_url,
                            "git_ref": draft.git_ref,
                            "nix_shell": draft.nix_shell,
                            "system_prompt": draft.system_prompt,
                            "tools": draft.tools,
                        });
                        let res = match &profile_id {
                            Some(id) => http_json(
                                &cfg,
                                "PATCH",
                                &format!("/profiles/update?id={id}"),
                                Some(&body),
                            )
                            .map(|_| id.clone()),
                            None => http_json(&cfg, "POST", "/profiles", Some(&body)).map(|v| {
                                v.pointer("/profile/id")
                                    .or_else(|| v.get("id"))
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("")
                                    .to_string()
                            }),
                        };
                        match res {
                            Ok(id) => write_frame(
                                &pipe,
                                &Frame::ProfilePutOk {
                                    req_id,
                                    profile_id: id,
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
                    ForgeJob::ProfileDelete { req_id, profile_id } => {
                        match http_json(&cfg, "DELETE", &format!("/profiles/{profile_id}"), None) {
                            Ok(_) => write_frame(&pipe, &Frame::ProfileDeleteOk { req_id }),
                            Err(e) => write_frame(
                                &pipe,
                                &Frame::Error {
                                    req_id: Some(req_id),
                                    message: e,
                                },
                            ),
                        }
                    }
                    // ----- agent registry (Herd H1) -----
                    ForgeJob::AgentList { req_id } => {
                        eprintln!("DEBUG: worker AgentList arm {req_id}");
                        match http_json(&cfg, "GET", "/agents", None) {
                            Ok(v) => {
                                let arr = v
                                    .get("agents")
                                    .and_then(|x| x.as_array())
                                    .cloned()
                                    .or_else(|| v.as_array().cloned())
                                    .unwrap_or_default();
                                let agents: Vec<ranch_protocol::AgentInfo> = arr
                                    .iter()
                                    .filter_map(|a| {
                                        Some(ranch_protocol::AgentInfo {
                                            id: a.get("id")?.as_str()?.to_string(),
                                            name: a
                                                .get("name")
                                                .and_then(|n| n.as_str())
                                                .unwrap_or("")
                                                .to_string(),
                                            avatar_url: a
                                                .get("avatar_url")
                                                .and_then(|x| x.as_str())
                                                .map(String::from),
                                            home_machine: a
                                                .get("home_machine")
                                                .and_then(|x| x.as_str())
                                                .map(String::from),
                                            // TODO(H5): derive from the agent's
                                            // most-active conversation
                                            // (GET /agents/:id/conversations?latest=1
                                            // per agent — not worth N round
                                            // trips on a list call in H1)
                                            last_active: None,
                                            conversation_count: None,
                                        })
                                    })
                                    .collect();
                                write_frame(&pipe, &Frame::AgentListOk { req_id, agents });
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
                    ForgeJob::AgentBusy { req_id, agent } => {
                        match http_json(&cfg, "GET", &format!("/agents/{agent}/active"), None) {
                            Ok(v) => write_frame(
                                &pipe,
                                &Frame::AgentBusyOk {
                                    req_id,
                                    busy: v.get("busy").and_then(|b| b.as_bool()).unwrap_or(false),
                                    current_conversation: v
                                        .get("current_conversation")
                                        .and_then(|c| c.as_str())
                                        .map(String::from),
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
                },
                Err(mpsc::RecvError) => return,
            }
        }
    });
}

/// Fetch the session's context-window usage (GET /sessions/{id}/context)
/// and broadcast it as `meta { kind: "context", status: "31% · 62k/200k" }`
/// (estimate fallback: `"~62k est."`). Runs on its own thread — blocking
/// HTTP must never stall the poll loop.
fn emit_context(pipe: &PipeWriter, cfg: &ForgeConfig, pane: Uuid, forge_sid: Uuid) {
    let Ok(mut res) = ureq::get(&format!("{}/sessions/{forge_sid}/context", cfg.base))
        .header("X-API-Key", &cfg.key)
        .call()
    else {
        return;
    };
    let mut text = String::new();
    if res
        .body_mut()
        .as_reader()
        .read_to_string(&mut text)
        .is_err()
    {
        return;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    let status = match (v.get("source"), v.get("percent")) {
        (Some(serde_json::Value::String(s)), p) if s == "live" => {
            let tokens = v.get("tokens").and_then(|t| t.as_i64());
            let window = v.get("context_window").and_then(|w| w.as_i64());
            let pct = p.and_then(|x| x.as_f64());
            match (tokens, window, pct) {
                (Some(t), Some(w), Some(p)) => {
                    format!("ctx {p:.0}% · {}/{}", format_k(t), format_k(w))
                }
                _ => format!("ctx ~{} est.", format_k(tokens.unwrap_or(0))),
            }
        }
        _ => format!(
            "ctx ~{} est.",
            format_k(v.get("tokens").and_then(|t| t.as_i64()).unwrap_or(0))
        ),
    };
    write_frame(
        pipe,
        &Frame::Meta {
            session: String::new(),
            pane: Some(pane.to_string()),
            kind: "context".into(),
            status: Some(status),
        background: None,
        },
    );
}

/// Human-friendly token count (12345 → "12k").
fn format_k(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1000 {
        format!("{}k", n / 1000)
    } else {
        n.to_string()
    }
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
    use std::io::Read as _;
    let url = format!("{}{}", cfg.base, path);
    let m = method.to_ascii_uppercase();
    // Agent with status-as-error OFF so 4xx/5xx responses come back as
    // readable responses instead of Error::StatusCode(code) — that lets
    // us surface Forge's actual error body (e.g. "missing field
    // `working_dir`", "tools: invalid type") instead of a bare "422".
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build(),
    );
    let sent = match (m.as_str(), body) {
        ("GET", _) => agent.get(&url).header("X-API-Key", &cfg.key).call(),
        ("POST", b) => agent
            .post(&url)
            .header("X-API-Key", &cfg.key)
            .send_json(b.cloned().unwrap_or(serde_json::Value::Null)),
        ("PATCH", b) => agent
            .patch(&url)
            .header("X-API-Key", &cfg.key)
            .send_json(b.cloned().unwrap_or(serde_json::Value::Null)),
        _ => return Err(format!("forge: unsupported method {method}")),
    };
    let mut res = sent.map_err(|e| format!("forge {method} {path}: {e}"))?;
    let mut text = String::new();
    res.body_mut()
        .as_reader()
        .read_to_string(&mut text)
        .map_err(|e| format!("forge read {path}: {e}"))?;
    // 4xx/5xx: report the status + Forge's error body, and stop here.
    if !res.status().is_success() {
        let detail = text.trim();
        return Err(if detail.is_empty() {
            format!("forge {method} {path}: HTTP {}", res.status())
        } else {
            format!(
                "forge {method} {path}: HTTP {} \u{2014} {detail}",
                res.status()
            )
        });
    }
    serde_json::from_str(&text).map_err(|e| format!("forge decode {path}: {e}"))
}

/// Fetch pi's `models.json` catalog via forge's
/// `GET /v1/models/catalog` (secrets already stripped). Returns the
/// flattened model list plus a (provider, id) -> name lookup map.
fn fetch_catalog(
    cfg: &ForgeConfig,
) -> (
    Vec<ModelChoice>,
    std::collections::HashMap<(String, String), String>,
) {
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
    profile_override: Option<&str>,
) -> Result<Uuid, String> {
    // profile: explicit override > configured > the first one
    let profile_id = match profile_override {
        Some(p) => p.to_string(),
        None => match &cfg.profile {
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
        },
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

// ----- agent registry (Herd H1): sync helpers for the main loop -----
//
// The same sync-localhost pattern as `create_forge_session`: the
// daemon's AgentTalk arm orchestrates a couple of one-shot calls and
// then creates the pane locally (the forge worker is for long-lived
// jobs — watches + request/reply broadcasts — pane creation belongs
// to the daemon).

/// GET /agents/{id} → the agent row (404 → "unknown agent" error).
pub fn agent_get(cfg: &ForgeConfig, agent: &str) -> Result<serde_json::Value, String> {
    let v = http_json(cfg, "GET", &format!("/agents/{agent}"), None)?;
    let a = v
        .get("agent")
        .ok_or("GET /agents/{agent}: unexpected shape")?;
    Ok(a.clone())
}

/// The owner-visible agent list (H5.3): `GET /agents` →
/// (id → name). Used by the activity poller + the pause path when the
/// daemon's cached agent map is empty (fresh boot, no AgentList yet).
pub fn agent_list_names(
    cfg: &ForgeConfig,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let v = http_json(cfg, "GET", "/agents", None)?;
    let arr = v
        .get("agents")
        .and_then(|x| x.as_array())
        .cloned()
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();
    let mut out = std::collections::BTreeMap::new();
    for a in arr {
        if let (Some(id), Some(name)) = (
            a.get("id").and_then(|x| x.as_str()),
            a.get("name").and_then(|x| x.as_str()),
        ) {
            out.insert(id.to_string(), name.to_string());
        }
    }
    Ok(out)
}

/// Resolve the display name of the agent that owns a forge session
/// (for the `AgentNotify` frame the `@notify:` message spawns):
/// session row -> `agent_id` -> agents row `name`. Falls back to the
/// session title, then to "agent". One thread, two localhost GETs.
fn notify_agent_name(cfg: &ForgeConfig, forge_sid: Uuid) -> String {
    let s = http_json(cfg, "GET", &format!("/sessions/{forge_sid}"), None)
        .ok()
        .and_then(|v| v.get("session").cloned());
    if let Some(s) = s {
        if let Some(aid) = s.get("agent_id").and_then(|x| x.as_str())
            && !aid.is_empty()
            && let Ok(a) = agent_get(cfg, aid)
            && let Some(n) = a.get("name").and_then(|x| x.as_str())
            && !n.is_empty()
        {
            return n.to_string();
        }
        if let Some(t) = s.get("title").and_then(|x| x.as_str()) && !t.is_empty() {
            return t.to_string();
        }
    }
    "agent".to_string()
}

/// `POST /agents/{agent}/pause` | `/resume` (H5.3 kill switch) →
/// `{id, paused}`. Idempotent on the forge side; returns the
/// resulting paused state.
pub fn agent_set_paused(cfg: &ForgeConfig, agent: &str, paused: bool) -> Result<bool, String> {
    let path = if paused {
        format!("/agents/{agent}/pause")
    } else {
        format!("/agents/{agent}/resume")
    };
    let v = http_json(cfg, "POST", &path, Some(&serde_json::json!({})))?;
    v.get("paused")
        .and_then(|x| x.as_bool())
        .ok_or(format!("forge POST {path}: unexpected response shape"))
}

/// The agent's most-active conversation (GET /agents/{id}/conversations
/// ?latest=1 → first row's id; None when the agent has none).
pub fn agent_latest_conversation(cfg: &ForgeConfig, agent: &str) -> Result<Option<Uuid>, String> {
    let v = http_json(
        cfg,
        "GET",
        &format!("/agents/{agent}/conversations?latest=1"),
        None,
    )?;
    let arr = v
        .get("conversations")
        .and_then(|x| x.as_array())
        .cloned()
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();
    let id = arr
        .first()
        .and_then(|s| s.get("id"))
        .and_then(|i| i.as_str())
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|e| format!("agent conversation id: {e}"))?;
    Ok(id)
}

/// POST /agents/{id}/conversations { title?, fork_from? } → the new
/// forge session id (201 body: {session: {...}, working_dir}).
pub fn agent_create_conversation(
    cfg: &ForgeConfig,
    agent: &str,
    title: Option<&str>,
    fork_from: Option<Uuid>,
) -> Result<Uuid, String> {
    let mut body = serde_json::json!({});
    if let Some(t) = title.filter(|t| !t.is_empty()) {
        body["title"] = serde_json::Value::String(t.to_string());
    }
    if let Some(f) = fork_from {
        body["fork_from"] = serde_json::Value::String(f.to_string());
    }
    let v = http_json(
        cfg,
        "POST",
        &format!("/agents/{agent}/conversations"),
        Some(&body),
    )?;
    let sess = v
        .get("session")
        .ok_or("POST /agents/{agent}/conversations: no session")?;
    let id = sess
        .get("id")
        .and_then(|i| i.as_str())
        .ok_or("POST /agents/{agent}/conversations: no id")?;
    Uuid::parse_str(id).map_err(|e| format!("forge conversation id: {e}"))
}

/// Decide how AgentTalk resolves a conversation (pure — unit-tested):
/// continue-with-none falls back to creating, fork-without-source
/// falls back to the latest (or fails when the agent has none).
#[derive(Debug, Clone, PartialEq)]
pub enum TalkPlan {
    /// attach the existing conversation
    Attach(Uuid),
    /// create one first (title + optional fork point)
    Create {
        title: Option<String>,
        fork_from: Option<Uuid>,
    },
}

pub fn talk_plan(
    mode: &str,
    latest: Option<Uuid>,
    fork_from: Option<Uuid>,
    title: Option<String>,
) -> Result<TalkPlan, String> {
    match mode {
        "continue" => match latest {
            Some(l) => Ok(TalkPlan::Attach(l)),
            None => Ok(TalkPlan::Create {
                title,
                fork_from: None,
            }),
        },
        "new" => Ok(TalkPlan::Create {
            title,
            fork_from: None,
        }),
        "fork" => {
            let src = fork_from.or(latest).ok_or_else(|| {
                "nothing to fork: the agent has no conversation (talk to it first, or mode:new)"
                    .to_string()
            })?;
            Ok(TalkPlan::Create {
                title,
                fork_from: Some(src),
            })
        }
        other => Err(format!(
            "unknown agent talk mode {other:?} (continue|new|fork)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    // ---------- memory_review card answer → result payload (H4.4) ----------

    #[test]
    fn memory_review_reply_maps_choices_to_belief_actions() {
        // Keep (choice 0)
        let r = memory_review_reply(&[0], "");
        assert_eq!(r["success"], true);
        assert_eq!(r["output"]["action"], "keep");
        // Forget (choice 1)
        let r = memory_review_reply(&[1], "");
        assert_eq!(r["success"], true);
        assert_eq!(r["output"]["action"], "forget");
        // Free text IS the Edit — the text becomes the new content,
        // and it takes precedence over any co-selected choice.
        let r = memory_review_reply(&[0], "user prefers VERY brief replies");
        assert_eq!(r["success"], true);
        assert_eq!(r["output"]["action"], "edit");
        assert_eq!(r["output"]["content"], "user prefers VERY brief replies");
        let r = memory_review_reply(&[1], " deploys go on Thursdays ");
        assert_eq!(r["output"]["action"], "edit");
        assert_eq!(r["output"]["content"], "deploys go on Thursdays");
        // Blank text + a choice → the choice wins (no stray edit).
        assert_eq!(memory_review_reply(&[0], "   ")["output"]["action"], "keep");
        // Nothing selected, nothing typed → failed relay (the belief
        // stays pending on the forge side).
        let r = memory_review_reply(&[], "");
        assert_eq!(r["success"], false);
        assert!(r["error"].as_str().unwrap().contains("no answer in time"));
    }

    // ---------- research_report card answer → result payload (H5.1) ----------

    #[test]
    fn research_report_reply_maps_choices_to_actions() {
        // Use (choice 0)
        let r = research_report_reply(&[0], "");
        assert_eq!(r["success"], true);
        assert_eq!(r["output"]["action"], "use");
        // Discard (choice 1)
        let r = research_report_reply(&[1], "");
        assert_eq!(r["output"]["action"], "discard");
        // Ask more (choice 2) + the follow-up text
        let r = research_report_reply(&[2], " dig deeper into the security model ");
        assert_eq!(r["success"], true);
        assert_eq!(r["output"]["action"], "ask_more");
        assert_eq!(
            r["output"]["followup"],
            "dig deeper into the security model"
        );
        // Free text WITHOUT any choice is a follow-up ("dig further")
        let r = research_report_reply(&[], "what about offline mode?");
        assert_eq!(r["output"]["action"], "ask_more");
        assert_eq!(r["output"]["followup"], "what about offline mode?");
        // "Ask more" with blank text is NOT a follow-up → failed relay
        let r = research_report_reply(&[2], "   ");
        assert_eq!(r["success"], false);
        // Nothing selected, nothing typed → failed relay (the row
        // stays `done` on the forge side, still open).
        let r = research_report_reply(&[], "");
        assert_eq!(r["success"], false);
        assert!(r["error"].as_str().unwrap().contains("no answer in time"));
    }

    // ---------- talk_plan (pure mode-resolution logic) ----------

    #[test]
    fn talk_plan_continue_uses_latest() {
        let l = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        assert_eq!(
            talk_plan("continue", Some(l), None, None),
            Ok(TalkPlan::Attach(l))
        );
    }

    #[test]
    fn talk_plan_continue_without_latest_creates() {
        assert_eq!(
            talk_plan("continue", None, None, Some("t".into())),
            Ok(TalkPlan::Create {
                title: Some("t".into()),
                fork_from: None,
            })
        );
    }

    #[test]
    fn talk_plan_new_ignores_latest_and_fork() {
        let l = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        let f = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();
        assert_eq!(
            talk_plan("new", Some(l), Some(f), None),
            Ok(TalkPlan::Create {
                title: None,
                fork_from: None,
            })
        );
    }

    #[test]
    fn talk_plan_fork_prefers_explicit_source() {
        let l = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        let f = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();
        assert_eq!(
            talk_plan("fork", Some(l), Some(f), None),
            Ok(TalkPlan::Create {
                title: None,
                fork_from: Some(f),
            })
        );
    }

    #[test]
    fn talk_plan_fork_falls_back_to_latest() {
        let l = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        assert_eq!(
            talk_plan("fork", Some(l), None, None),
            Ok(TalkPlan::Create {
                title: None,
                fork_from: Some(l),
            })
        );
    }

    #[test]
    fn talk_plan_fork_with_no_conversation_errors() {
        let e = talk_plan("fork", None, None, None).unwrap_err();
        assert!(e.contains("nothing to fork"), "{e}");
    }

    #[test]
    fn talk_plan_rejects_unknown_mode() {
        assert!(talk_plan("teleport", None, None, None).is_err());
    }

    // ---------- sync HTTP helpers, against a one-shot mock server ----------

    /// One-shot mock: answers exactly one request with canned JSON,
    /// capturing `METHOD path body` for assertions.
    fn mock_forge(canned: &str, seen: Arc<Mutex<String>>) -> ForgeConfig {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let canned = canned.to_string();
        let seen2 = seen.clone();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 4096];
            // read until we have the header block + the body it promises
            loop {
                let n = stream.read(&mut tmp).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = find_bytes(&buf, b"\r\n\r\n") {
                    let hdr = String::from_utf8_lossy(&buf[..pos]).to_string();
                    let clen = hdr
                        .lines()
                        .find_map(|l| {
                            let low = l.to_lowercase();
                            low.strip_prefix("content-length:")
                                .map(|v| v.trim().to_string())
                        })
                        .and_then(|l| l.parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= pos + 4 + clen {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&buf).to_string();
            let mut lines = text.lines();
            let reqline = lines.next().unwrap_or_default();
            let mut parts = reqline.split_whitespace();
            let method = parts.next().unwrap_or_default().to_string();
            let path = parts.next().unwrap_or_default().to_string();
            let body = text
                .split_once("\r\n\r\n")
                .map(|(_, b)| b.to_string())
                .unwrap_or_default();
            *seen2.lock().unwrap() = format!("{method} {path} {body}");
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{canned}",
                canned.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        });
        ForgeConfig {
            base: format!("http://127.0.0.1:{port}"),
            key: "test-key".into(),
            profile: None,
        }
    }

    fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    #[test]
    fn agent_get_parses_row() {
        let seen = Arc::new(Mutex::new(String::new()));
        let cfg = mock_forge(
            r#"{"agent":{"id":"a","name":"iris","home_machine":"box1","avatar_url":null}}"#,
            seen.clone(),
        );
        let v = agent_get(&cfg, "a").unwrap();
        assert_eq!(v["name"], "iris");
        assert_eq!(v["home_machine"], "box1");
        let s = seen.lock().unwrap().clone();
        assert!(s.starts_with("GET /agents/a"), "{s}");
    }

    #[test]
    fn agent_latest_conversation_first_row() {
        let seen = Arc::new(Mutex::new(String::new()));
        let cfg = mock_forge(
            r#"{"conversations":[{"id":"11111111-1111-1111-1111-111111111111","last_active":"2026-10-07T00:00:00Z"},{"id":"22222222-2222-2222-2222-222222222222"}]}"#,
            seen.clone(),
        );
        let got = agent_latest_conversation(&cfg, "a").unwrap();
        assert_eq!(
            got,
            Some(Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap())
        );
        let s = seen.lock().unwrap().clone();
        assert!(s.contains("/conversations?latest=1"), "{s}");
    }

    #[test]
    fn agent_latest_conversation_none_when_empty() {
        let seen = Arc::new(Mutex::new(String::new()));
        let cfg = mock_forge(r#"{"conversations":[]}"#, seen.clone());
        assert_eq!(agent_latest_conversation(&cfg, "a").unwrap(), None);
    }

    #[test]
    fn agent_create_conversation_posts_fork_and_title() {
        let seen = Arc::new(Mutex::new(String::new()));
        let cfg = mock_forge(
            r#"{"session":{"id":"33333333-3333-3333-3333-333333333333"},"working_dir":"/w"}"#,
            seen.clone(),
        );
        let fork = Uuid::parse_str("44444444-4444-4444-4444-444444444444").unwrap();
        let got = agent_create_conversation(&cfg, "a", Some("branch"), Some(fork)).unwrap();
        assert_eq!(got.to_string(), "33333333-3333-3333-3333-333333333333");
        let s = seen.lock().unwrap().clone();
        assert!(s.starts_with("POST /agents/a/conversations"), "{s}");
        assert!(s.contains(&fork.to_string()), "fork_from uuid in body: {s}");
        assert!(s.contains("branch"), "title in body: {s}");
    }

    #[test]
    fn agent_create_conversation_empty_body_for_plain_new() {
        let seen = Arc::new(Mutex::new(String::new()));
        let cfg = mock_forge(
            r#"{"session":{"id":"33333333-3333-3333-3333-333333333333"}}"#,
            seen.clone(),
        );
        let got = agent_create_conversation(&cfg, "a", None, None).unwrap();
        assert_eq!(got.to_string(), "33333333-3333-3333-3333-333333333333");
        let s = seen.lock().unwrap().clone();
        assert!(s.ends_with(" {}"), "no title/fork → empty object body: {s}");
    }

    // ---------- forge-sse reconnect dedup (Herd H2.6) ----------
    //
    // Post-cutover, harness restarts are frequent and harmless by
    // design: the worker's SSE connection drops, and the reconnect's
    // catch-up replay re-sends rows that were already delivered on the
    // first connection (forge re-sends rows > `since` and the worker
    // dedupes by the `last_seq` high-water mark). These tests drive
    // `handle_event` — what the SSE loop calls per frame — across a
    // simulated drop + replay and assert no row is ever re-emitted as
    // a `chat` frame.

    /// A WatchState + pipe (a real file) + its path, for one test.
    fn sse_fixture() -> (Arc<WatchState>, PipeWriter, std::path::PathBuf) {
        let state = Arc::new(WatchState {
            pane: Uuid::new_v4(),
            forge_sid: Uuid::new_v4(),
            last_seq: AtomicI64::new(0),
            stop: AtomicBool::new(false),
            turn_gen: AtomicU64::new(0),
            background_turn: AtomicBool::new(false),
        });
        let path = std::env::temp_dir().join(format!("ranch-forge-sse-test-{}", Uuid::new_v4()));
        let file = std::fs::File::create(&path).expect("create pipe file");
        let pipe: PipeWriter = Arc::new(Mutex::new(file));
        (state, pipe, path)
    }

    fn message_event(seq: i64, role: &str, text: &str) -> String {
        serde_json::json!({
            "sequence": seq,
            "role": role,
            "content": text,
            "tool_name": null,
            "tool_call_id": null,
            "tool_output": null,
            "tool_input": null,
            "duration_ms": null,
            "created_at": "2026-10-08T00:00:00Z",
        })
        .to_string()
    }

    /// Decode every frame the fixture's pipe received, in order.
    fn pipe_frames(path: &std::path::Path) -> Vec<Frame> {
        std::fs::read_to_string(path)
            .expect("read pipe file")
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str::<Frame>(l).unwrap_or_else(|e| panic!("decode {l}: {e}")))
            .collect()
    }

    #[test]
    fn sse_reconnect_replay_is_deduped() {
        let (state, pipe, path) = sse_fixture();
        let cfg = ForgeConfig {
            base: "http://127.0.0.1:0".into(),
            key: "k".into(),
            profile: None,
        };

        // Connection 1: the stream delivers rows 1..3.
        for (seq, role, text) in [
            (1i64, "user", "legacy hello"),
            (2, "assistant", "legacy answer"),
            (3, "user", "next question"),
        ] {
            handle_event(
                &state,
                &cfg,
                &pipe,
                "message",
                &message_event(seq, role, text),
            );
        }
        assert_eq!(state.last_seq.load(Ordering::Relaxed), 3);

        // --- the harness restarts: the connection drops, and the
        // reconnect's catch-up REPLAYS rows 1..3 plus fresh rows 4..5
        // (the fork window is longer than the client's backoff).
        for (seq, role, text) in [
            (1i64, "user", "legacy hello"),
            (2, "assistant", "legacy answer"),
            (3, "user", "next question"),
            (4, "user", "reconnect question"),
            (5, "assistant", "reconnect answer"),
        ] {
            handle_event(
                &state,
                &cfg,
                &pipe,
                "message",
                &message_event(seq, role, text),
            );
        }

        assert_eq!(state.last_seq.load(Ordering::Relaxed), 5);
        let frames = pipe_frames(&path);
        let chat_texts: Vec<String> = frames
            .iter()
            .filter_map(|f| match f {
                Frame::Chat { msgs, .. } => {
                    Some(msgs.iter().map(|m| m.text.clone()).collect::<Vec<_>>())
                }
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(
            chat_texts,
            vec![
                "legacy hello",
                "legacy answer",
                "next question",
                "reconnect question",
                "reconnect answer"
            ],
            "replayed rows must never be re-emitted"
        );

        // An out-of-order replay of an already-delivered row that
        // arrives AFTER the high-water mark advanced is dropped too.
        let n_before = pipe_frames(&path).len();
        handle_event(
            &state,
            &cfg,
            &pipe,
            "message",
            &message_event(2, "assistant", "stale replay"),
        );
        assert_eq!(
            state.last_seq.load(Ordering::Relaxed),
            5,
            "a duplicate must not move the high-water mark backward"
        );
        assert_eq!(
            pipe_frames(&path).len(),
            n_before,
            "stale row emitted a frame"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sse_heartbeat_and_lagged_are_noops() {
        let (state, pipe, path) = sse_fixture();
        let cfg = ForgeConfig {
            base: "http://127.0.0.1:0".into(),
            key: "k".into(),
            profile: None,
        };
        handle_event(&state, &cfg, &pipe, "heartbeat", "");
        handle_event(&state, &cfg, &pipe, "lagged", "");
        handle_event(&state, &cfg, &pipe, "", "{}");
        assert!(
            pipe_frames(&path).is_empty(),
            "heartbeat/lagged/unknown events must write no frames"
        );
        assert_eq!(state.last_seq.load(Ordering::Relaxed), 0);
        let _ = std::fs::remove_file(&path);
    }
}
