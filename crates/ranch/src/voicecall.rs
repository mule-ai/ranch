//! `ranch call [agent]` — Herd H6.3 voice loop v1 (push-to-talk call).
//!
//! Transport decision (documented; mule spec.md "Voice loop (H6.3)"):
//! audio flows host ⇄ MULE directly — this CLI creates the mule job
//! via the mule jobs API (base + key read from the same
//! `~/.config/ranch/daemon.toml` the daemon reads; no new daemon
//! plumbing, no new wire frames — PROTOCOL.md is unchanged for H6.3)
//! and then dials the two per-call WebSockets:
//!
//! - IN:  `ws://mule/voice/{job}/in` — binary frames of 16 kHz mono
//!   s16le PCM (~30 ms each). Text frame `{"hangup":true}` hangs up;
//!   `{"text": …}` is the key-typed-text fallback (one utterance per
//!   line, no STT). The in socket closing also hangs up the call.
//! - OUT: `ws://mule/voice/{job}/out` — binary TTS chunks
//!   (tts_sample_rate from the step config, v1 24 kHz PCM) and text
//!   frames `{"assistant": …}` (per-turn transcript) + `{"done":true}`.
//!
//! Audio in: `arecord`/`parecord` when present on PATH; otherwise the
//! typed-text fallback (plan's "audio in via mic if available else
//! key-typed-text fallback so TUI parity is real, not fake"). Audio
//! out: `aplay`/`paplay`; when neither exists, the `{"assistant":…}`
//! frames are printed — a call with no speaker still works.
//!
//! Hang-up: "bye"/"exit" (text mode), Ctrl-C (kills the process → the
//! in socket closes → mule treats the close as a hang-up), or mic EOF.
//! All of them end the job COMPLETED. A mule-side crash fails the job;
//! the surfaces render that as "call dropped".

use std::io::{BufRead, BufReader, Read, Write};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use tungstenite::{Message, WebSocket};

use crate::daemon::mule::{MuleConfig, load_mule_config};

/// Sync blocking WebSocket (the same shape `CloudLink` uses).
type Ws = WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

/// 30 ms of 16 kHz mono s16le = 480 samples = 960 bytes.
const IN_FRAME_BYTES: usize = 960;

/// `ranch call [agent]` entry point (client dispatch).
pub fn run(agent_arg: Option<String>) {
    let Some(agent) = resolve_agent(agent_arg) else {
        return;
    };
    let cfg = match load_mule_config() {
        Some(c) => c,
        None => die_mule_unconfigured(),
    };
    let Some((workflow_id, tts_rate)) = pick_voice_workflow(&cfg, &agent) else {
        die_no_workflow(&agent);
    };
    let job_id = create_job(&cfg, &workflow_id);

    eprintln!("ranch: calling {} (job {job_id})", agent.name);
    eprintln!("ranch: mule {} · TTS out {tts_rate} Hz pcm", cfg.base);

    // ---------- out socket (reader thread) ----------
    let player_cmd = pick_out_player(tts_rate);
    eprintln!(
        "ranch: {}",
        match &player_cmd {
            Some(cmd) => format!("speaker: {} @ {tts_rate} Hz", cmd[0]),
            None => "speaker: none (aplay/paplay not found) — printing agent text only".into(),
        }
    );
    let out_done = Arc::new(AtomicBool::new(false));
    let has_speaker = player_cmd.is_some();
    let out_url = ws_out_url(&cfg.base, &job_id);
    let out_done_c = out_done.clone();
    let out = std::thread::spawn(move || {
        let ws = match tungstenite::connect(out_url) {
            Ok((ws, _)) => ws,
            Err(e) => {
                eprintln!("ranch: out socket: {e}");
                out_done_c.store(true, Ordering::SeqCst);
                return;
            }
        };
        let mut player_stdin = player_cmd.as_ref().and_then(|c| spawn_player(c));
        let mut ws = ws;
        loop {
            match ws.read() {
                Ok(Message::Binary(data)) => {
                    if let Some(stdin) = player_stdin.as_mut() {
                        let _ = stdin.write_all(&data);
                    }
                }
                Ok(Message::Text(text)) => {
                    let v: Value = match serde_json::from_str(text.as_str()) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if v.get("done").and_then(Value::as_bool) == Some(true) {
                        break;
                    }
                    if let Some(a) = v.get("assistant").and_then(Value::as_str) {
                        eprintln!("agent: {a}");
                    }
                }
                Ok(_) => {}
                // EOF surfaces as a ConnectionClosed error on the
                // sync client — the call ended mule-side.
                Err(_) => break,
            }
        }
        out_done_c.store(true, Ordering::SeqCst);
    });

    // ---------- in socket + audio in ----------
    let in_url = ws_in_url(&cfg.base, &job_id);
    let mut in_ws = match tungstenite::connect(in_url) {
        Ok((ws, _)) => ws,
        Err(e) => {
            eprintln!("ranch: in socket: {e}");
            std::process::exit(1);
        }
    };
    run_audio_in(&mut in_ws, &out_done, has_speaker);

    // Hang up (explicit frame; the close below is the belt-and-braces
    // — mule hangs up on either).
    let _ = in_ws.send(Message::Text(r#"{"hangup":true}"#.to_string().into()));
    let _ = in_ws.close(None);

    // Give the out thread a moment to flush the player + done marker.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !out_done.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = out.join();
    eprintln!("ranch: call ended");
}

/// Resolve the call target: agent name (case-insensitive, the
/// registry's unique-per-owner name) or agent id.
fn resolve_agent(arg: Option<String>) -> Option<ranch_protocol::AgentInfo> {
    let q = match arg {
        Some(q) => q,
        None => {
            eprintln!("usage: ranch call <agent>");
            return None;
        }
    };
    let agents = match crate::client::agent_list() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            return None;
        }
    };
    let found = agents
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case(&q) || a.id == q)
        .cloned();
    match found {
        Some(a) => Some(a),
        None => {
            eprintln!("ranch: no agent named {q:?} in the registry");
            for a in &agents {
                eprintln!("  {}", a.name);
            }
            None
        }
    }
}

fn die_mule_unconfigured() -> ! {
    eprintln!(
        "error: mule is not configured — set mule_url (+ mule_api_key) in \
         ~/.config/ranch/daemon.toml, then retry (H6.3: the call audio flows \
         host ⇄ mule directly)"
    );
    std::process::exit(1);
}

fn die_no_workflow(agent: &ranch_protocol::AgentInfo) -> ! {
    eprintln!(
        "error: no mule voice_call workflow found for agent '{}' — seed one \
         (mule examples/voice-loop/seed.sh AGENT_ID={})",
        agent.name, agent.id
    );
    std::process::exit(1);
}

/// Find the voice_call workflow that serves this agent: list the
/// workflows, inspect each step set, and pick the `voice_call` step
/// whose config.agent_id is the agent. Returns (workflow_id, tts rate).
fn pick_voice_workflow(
    cfg: &MuleConfig,
    agent: &ranch_protocol::AgentInfo,
) -> Option<(String, u32)> {
    let wf = http_json(cfg, "GET", "/api/v1/workflows", None).ok()?;
    for item in wf.as_array()?.iter() {
        let Some(id) = item.get("id")?.as_str() else {
            continue;
        };
        let Ok(steps) = http_json(cfg, "GET", &format!("/api/v1/workflows/{id}/steps"), None)
        else {
            continue;
        };
        for step in steps.as_array().into_iter().flatten() {
            if step.get("type").and_then(Value::as_str) != Some("voice_call") {
                continue;
            }
            let config = step.get("config").unwrap_or(&Value::Null);
            let target = config.get("agent_id").and_then(Value::as_str).unwrap_or("");
            if target == agent.id {
                return Some((id.to_string(), tts_rate_from_config(config)));
            }
        }
    }
    None
}

/// Out-socket sample rate from the step config (default 24 kHz —
/// Kokoro's PCM default; the out socket carries no rate metadata).
fn tts_rate_from_config(config: &Value) -> u32 {
    config
        .get("tts_sample_rate")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0 && *n <= 2_000_000)
        .map(|n| n as u32)
        .unwrap_or(24_000)
}

/// POST /api/v1/jobs {workflow_id} → the new job id.
fn create_job(cfg: &MuleConfig, workflow_id: &str) -> String {
    let body = serde_json::json!({ "workflow_id": workflow_id });
    let v = http_json(cfg, "POST", "/api/v1/jobs", Some(body)).unwrap_or_else(|e| {
        eprintln!("error: failed to start the call job: {e}");
        std::process::exit(1);
    });
    v.get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            eprintln!("error: mule did not return a job id: {v}");
            std::process::exit(1);
        })
}

/// mule base URL → in-socket WebSocket URL.
fn ws_in_url(base: &str, job_id: &str) -> String {
    format!("{}/voice/{job_id}/in", ws_base(base))
}

/// mule base URL → out-socket WebSocket URL.
fn ws_out_url(base: &str, job_id: &str) -> String {
    format!("{}/voice/{job_id}/out", ws_base(base))
}

fn ws_base(base: &str) -> String {
    match base.strip_prefix("https://") {
        Some(rest) => format!("wss://{}", rest.trim_end_matches('/')),
        None => format!(
            "ws://{}",
            base.strip_prefix("http://")
                .unwrap_or(base)
                .trim_end_matches('/')
        ),
    }
}

/// The out player command, when aplay/paplay is on PATH: raw s16le
/// mono PCM from our pipe at the negotiated TTS rate. If it dies
/// early, later writes just fail and are dropped (documented v1).
fn pick_out_player(tts_rate: u32) -> Option<Vec<String>> {
    for name in ["aplay", "paplay"] {
        if which(name) {
            return Some(vec![
                name.to_string(),
                "-f".into(),
                "S16_LE".into(),
                "-r".into(),
                tts_rate.to_string(),
                "-c".into(),
                "1".into(),
                "-t".into(),
                "raw".into(),
                "-".into(),
            ]);
        }
    }
    None
}

fn spawn_player(cmd: &[String]) -> Option<std::process::ChildStdin> {
    let mut child = std::process::Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()
}

/// The audio-in half: mic (arecord/parecord → 30 ms binary frames) or
/// the typed-text fallback (one utterance per line; "bye"/"exit"
/// hangs up; Ctrl-C kills the process and the in-socket close does
/// the hang-up).
fn run_audio_in(in_ws: &mut Ws, out_done: &AtomicBool, has_speaker: bool) {
    match pick_in_mic() {
        Some(cmd) => {
            let mut child = match std::process::Command::new(&cmd[0])
                .args(&cmd[1..])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    eprintln!(
                        "ranch: mic ({}) failed: {e} — falling back to typed text",
                        cmd[0]
                    );
                    typed_in(in_ws, out_done, has_speaker);
                    return;
                }
            };
            eprintln!(
                "ranch: mic: {name} — speak; Ctrl-C or mic EOF hangs up",
                name = cmd[0]
            );
            let stdout = child.stdout.take().expect("piped");
            let mut reader = BufReader::new(stdout);
            let mut frame = vec![0u8; IN_FRAME_BYTES];
            loop {
                if out_done.load(Ordering::SeqCst) {
                    break; // the out socket reported the call ended
                }
                // EOF/error: the mic went away → hang up.
                let Ok(()) = reader.read_exact(&mut frame) else {
                    break;
                };
                if in_ws.send(Message::binary(frame.to_vec())).is_err() {
                    eprintln!("ranch: in socket closed — ending call");
                    break;
                }
            }
            let _ = child.kill();
        }
        None => typed_in(in_ws, out_done, has_speaker),
    }
}

/// The key-typed-text fallback (TUI parity path): each non-empty line
/// is one utterance (sent as a `{"text":…}` frame — no STT hop);
/// "bye"/"exit" ends the call.
fn typed_in(in_ws: &mut Ws, out_done: &AtomicBool, has_speaker: bool) {
    if has_speaker {
        eprintln!(
            "ranch: no mic tool found (arecord/parecord) — typing mode: one utterance per line"
        );
    } else {
        eprintln!("ranch: typing mode (no mic, no speaker): type lines, agent replies are printed");
    }
    eprintln!("ranch: 'bye' or Ctrl-C hangs up");
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        print!("you> ");
        let _ = std::io::stderr().flush();
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => {
                eprintln!("ranch: stdin: {e}");
                break;
            }
        }
        let trimmed = line.trim();
        match in_frame_for_line(trimmed) {
            Some(InFrame::HangUp) => break,
            Some(InFrame::Text(s)) => {
                let payload = serde_json::json!({ "text": s });
                if in_ws
                    .send(Message::Text(
                        serde_json::to_string(&payload).unwrap().into(),
                    ))
                    .is_err()
                {
                    eprintln!("ranch: in socket closed — ending call");
                    break;
                }
            }
            None => {}
        }
        if out_done.load(Ordering::SeqCst) {
            break;
        }
    }
}

/// One in-socket action for a typed line.
#[derive(Debug)]
enum InFrame {
    Text(String),
    HangUp,
}

/// Pure mapping from a typed line to the in-socket action (unit-
/// tested): "bye"/"exit" (case-insensitive, whole line) hang up; other
/// non-empty lines are typed-utterance text frames; blank lines do
/// nothing.
fn in_frame_for_line(line: &str) -> Option<InFrame> {
    let l = line.trim();
    if l.is_empty() {
        return None;
    }
    if l.eq_ignore_ascii_case("bye") || l.eq_ignore_ascii_case("exit") {
        return Some(InFrame::HangUp);
    }
    Some(InFrame::Text(l.to_string()))
}

/// The mic capture command, when arecord/parecord is on PATH: raw
/// 16 kHz mono s16le, 960-byte (30 ms) blocks straight to stdout.
fn pick_in_mic() -> Option<Vec<String>> {
    for name in ["arecord", "parecord"] {
        if which(name) {
            return Some(vec![
                name.to_string(),
                "-f".into(),
                "S16_LE".into(),
                "-r".into(),
                "16000".into(),
                "-c".into(),
                "1".into(),
                "-B".into(),
                IN_FRAME_BYTES.to_string(),
                "-t".into(),
                "raw".into(),
                "-".into(),
            ]);
        }
    }
    None
}

/// PATH lookup without a `which` dependency.
fn which(name: &str) -> bool {
    let Ok(paths) = std::env::var("PATH") else {
        return false;
    };
    paths
        .split(':')
        .filter(|p| !p.is_empty())
        .any(|p| std::path::Path::new(p).join(name).is_file())
}

/// Minimal authenticated JSON helper for the mule jobs/workflows API
/// (same shape as the daemon-side mule worker's helper).
fn http_json(
    cfg: &MuleConfig,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<Value, String> {
    let url = format!("{}{}", cfg.base, path);
    fn with_auth<B>(r: ureq::RequestBuilder<B>, key: &Option<String>) -> ureq::RequestBuilder<B> {
        match key {
            Some(k) => r.header("Authorization", &format!("Bearer {k}")),
            None => r,
        }
    }
    let sent = match (method.to_ascii_uppercase().as_str(), body) {
        ("GET", _) => with_auth(ureq::get(&url), &cfg.api_key).call(),
        ("POST", b) => {
            with_auth(ureq::post(&url), &cfg.api_key).send_json(b.unwrap_or(Value::Null))
        }
        _ => return Err(format!("unsupported method {method}")),
    };
    let mut res = sent.map_err(|e| format!("mule {method} {path}: {e}"))?;
    let mut text = String::new();
    res.body_mut()
        .as_reader()
        .read_to_string(&mut text)
        .map_err(|e| format!("mule read {path}: {e}"))?;
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).map_err(|e| format!("mule decode {path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_frame_maps_bye_exit_to_hangup() {
        for line in ["bye", "BYE", "exit", " EXIT "] {
            match in_frame_for_line(line) {
                Some(InFrame::HangUp) => {}
                other => panic!("{line:?} -> {other:?}"),
            }
        }
    }

    #[test]
    fn in_frame_maps_other_lines_to_text() {
        match in_frame_for_line("check the deploy logs") {
            Some(InFrame::Text(s)) => assert_eq!(s, "check the deploy logs"),
            other => panic!("utterance -> {other:?}"),
        }
        assert!(in_frame_for_line("").is_none());
        assert!(in_frame_for_line("   ").is_none());
        // "exit" only hangs up as a whole line, not as a prefix:
        match in_frame_for_line("exit the build") {
            Some(InFrame::Text(s)) => assert_eq!(s, "exit the build"),
            other => panic!("'exit the build' -> {other:?}"),
        }
    }

    #[test]
    fn ws_urls_convert_http_to_ws() {
        assert_eq!(
            ws_in_url("http://10.0.0.5:8091", "j-1"),
            "ws://10.0.0.5:8091/voice/j-1/in"
        );
        // Trailing slash on the base must not double the path slash.
        assert_eq!(
            ws_out_url("http://10.0.0.5:8091/", "j-1"),
            "ws://10.0.0.5:8091/voice/j-1/out"
        );
        assert_eq!(
            ws_in_url("https://mule.example/", "j-2"),
            "wss://mule.example/voice/j-2/in"
        );
        assert_eq!(
            ws_out_url("https://mule.example/", "j-2"),
            "wss://mule.example/voice/j-2/out"
        );
    }

    #[test]
    fn tts_rate_defaults_and_bounds() {
        assert_eq!(tts_rate_from_config(&Value::Null), 24_000);
        assert_eq!(
            tts_rate_from_config(&serde_json::json!({"tts_sample_rate": 8000})),
            8000
        );
        // Out-of-range or wrong-typed values fall back rather than
        // build a broken player command.
        assert_eq!(
            tts_rate_from_config(&serde_json::json!({"tts_sample_rate": 0})),
            24_000
        );
        assert_eq!(
            tts_rate_from_config(&serde_json::json!({"tts_sample_rate": 3_000_000})),
            24_000
        );
        assert_eq!(
            tts_rate_from_config(&serde_json::json!({"tts_sample_rate": "fast"})),
            24_000
        );
    }

    #[test]
    fn mic_command_shape_when_present() {
        let Some(cmd) = pick_in_mic() else {
            eprintln!("(no arecord/parecord on this box — skipping shape check)");
            return;
        };
        assert!(cmd[0] == "arecord" || cmd[0] == "parecord");
        let joined = cmd.join(" ");
        assert!(joined.contains("-r 16000"), "{joined}");
        assert!(joined.contains("-c 1"), "{joined}");
        assert!(joined.contains(&format!("-B {IN_FRAME_BYTES}")), "{joined}");
        assert!(joined.ends_with("-t raw -"), "{joined}");
    }
}
