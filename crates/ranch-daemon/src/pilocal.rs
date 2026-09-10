//! Local `pi` agent backing for chat panes (M9).
//!
//! Same chat-pane UX as forge-backed panes, but the agent harness is a
//! local `pi --mode rpc` child process instead of the lab forge API:
//!
//! ```text
//! ChatSend ──► daemon: {"type":"prompt","message":...} ──► pi stdin
//! pi stdout ──► JSON events ──► Frame::Chat / Meta(working|idle) ──► pipe
//! ```
//!
//! The daemon-side pre-match resolves the pipe frames to the pane and
//! broadcasts to attached clients — identical to the forge flow, so
//! clients can't tell the difference (PaneSnap.kind stays "forge-chat";
//! `forge_session` is simply absent).

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ranch_protocol::{ChatMsg, Frame};
use uuid::Uuid;

use crate::forge::PipeWriter;

/// Monotonic message seq for locally-generated chat rows (per pane the
/// clients only need strictly-increasing; a process-wide counter is the
/// simplest way to guarantee it across panes).
static SEQ: AtomicU64 = AtomicU64::new(1);
fn next_seq() -> i64 {
    SEQ.fetch_add(1, Ordering::Relaxed) as i64
}

/// One local pi agent: the child process + its bookkeeping.
pub struct LocalPi {
    pub pane: Uuid,
    pub cwd: String,
    child: Mutex<Option<Child>>,
    stdin: Option<std::process::ChildStdin>,
    stop: Arc<AtomicBool>,
}

impl LocalPi {
    /// Spawn `pi --mode rpc` in `cwd`. Registers itself in `panes`;
    /// the stdout reader thread starts immediately and emits
    /// Chat/Meta frames into `pipe` (session left blank — the main
    /// loop's pre-match fills it and broadcasts).
    pub fn spawn(
        pane: Uuid,
        cwd: &str,
        pipe: PipeWriter,
        panes: &mut BTreeMap<Uuid, Arc<LocalPi>>,
    ) -> Result<(), String> {
        let mut child = Command::new("pi")
            .arg("--mode")
            .arg("rpc")
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stdin(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn pi: {e}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "pi stdin capture failed".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "pi stdout capture failed".to_string())?;

        let stop = Arc::new(AtomicBool::new(false));
        let lp = Arc::new(LocalPi {
            pane,
            cwd: cwd.to_string(),
            child: Mutex::new(Some(child)),
            stdin: Some(stdin),
            stop: stop.clone(),
        });
        panes.insert(pane, lp.clone());

        // reader thread: pi events -> chat frames
        let t_pane = pane;
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            let mut pending_tool: Option<(String, std::time::Instant)> = None;
            for line in reader.lines() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                let ev = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match ev {
                    "message_end" => {
                        // assistant replies land here with full content
                        if let Some(msg) = v.get("message") {
                            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
                            if role == "assistant" {
                                let text = msg
                                    .get("content")
                                    .and_then(|c| c.as_array())
                                    .map(|blocks| {
                                        blocks
                                            .iter()
                                            .filter_map(|b| {
                                                if b.get("type").and_then(|t| t.as_str())
                                                    == Some("text")
                                                {
                                                    b.get("text").and_then(|t| t.as_str())
                                                } else {
                                                    None
                                                }
                                            })
                                            .collect::<Vec<_>>()
                                            .join("")
                                    })
                                    .unwrap_or_default();
                                let trimmed = text.trim();
                                if !trimmed.is_empty() {
                                    emit_chat(&pipe, t_pane, "assistant", trimmed);
                                }
                            }
                        }
                    }
                    "tool_execution_start" => {
                        let name = v
                            .get("toolName")
                            .and_then(|t| t.as_str())
                            .unwrap_or("tool")
                            .to_string();
                        pending_tool = Some((name, std::time::Instant::now()));
                    }
                    "tool_execution_end" => {
                        if let Some((name, started)) = pending_tool.take() {
                            let dur = started.elapsed().as_millis() as i64;
                            let out = v
                                .get("result")
                                .map(|r| r.to_string())
                                .unwrap_or_default();
                            emit_tool(&pipe, t_pane, &name, dur, &out);
                        }
                    }
                    "turn_end" | "agent_end" => {
                        write_status(&pipe, "idle");
                    }
                    "error" => {
                        let msg = v
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("pi error");
                        emit_chat(&pipe, t_pane, "assistant", &format!("⚠ {msg}"));
                        write_status(&pipe, "idle");
                    }
                    _ => {}
                }
            }
            // stdout closed: pi is gone
            eprintln!("ranchd: local pi pane {t_pane} exited");
        });
        Ok(())
    }

    /// Send a user prompt: write the RPC prompt to pi's stdin first,
    /// then record the user row + flip the working indicator (so a
    /// failed write doesn't leave phantom rows).
    pub fn prompt(&self, pipe: &PipeWriter, text: &str) -> Result<(), String> {
        let stdin = self
            .stdin
            .as_ref()
            .ok_or_else(|| "pi stdin already taken".to_string())?;
        let mut s = stdin;
        let line = serde_json::json!({"type": "prompt", "message": text}).to_string();
        s.write_all(line.as_bytes())
            .and_then(|_| s.write_all(b"\n"))
            .and_then(|_| s.flush())
            .map_err(|e| format!("pi stdin: {e}"))?;
        // rows AFTER the write succeeded (a failed write emits the
        // error row + clears the indicator instead)
        emit_chat(pipe, self.pane, "user", text);
        write_status(pipe, "working");
        Ok(())
    }

    /// Kill the child (pane close). Best-effort.
    pub fn kill(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut guard) = self.child.lock() {
            if let Some(c) = guard.as_mut() {
                let _ = c.kill();
            }
            *guard = None;
        }
    }
}

fn emit_chat(pipe: &PipeWriter, pane: Uuid, role: &str, text: &str) {
    write_frame(
        pipe,
        &Frame::Chat {
            id: String::new(),
            session: String::new(),
            pane: pane.to_string(),
            msgs: vec![ChatMsg {
                seq: next_seq(),
                role: role.to_string(),
                text: text.to_string(),
                tool_name: None,
                tool_call_id: None,
                tool_output: None,
                duration_ms: None,
                created_at: None,
            }],
            reset: false,
        },
    );
}

fn emit_tool(pipe: &PipeWriter, pane: Uuid, name: &str, dur_ms: i64, out: &str) {
    write_frame(
        pipe,
        &Frame::Chat {
            id: String::new(),
            session: String::new(),
            pane: pane.to_string(),
            msgs: vec![ChatMsg {
                seq: next_seq(),
                role: "tool".to_string(),
                text: String::new(),
                tool_name: Some(name.to_string()),
                tool_call_id: None,
                tool_output: Some(out.to_string()),
                duration_ms: Some(dur_ms),
                created_at: None,
            }],
            reset: false,
        },
    );
}

fn write_status(pipe: &PipeWriter, status: &str) {
    write_frame(
        pipe,
        &Frame::Meta {
            session: String::new(),
            pane: None,
            kind: "agent".to_string(),
            status: Some(status.to_string()),
        },
    );
}

fn write_frame(pipe: &PipeWriter, frame: &Frame) {
    let cid = Uuid::new_v4().to_string();
    if let Ok(mut f) = pipe.lock() {
        use std::io::Write as _;
        for line in ranch_protocol::encode_frame(frame, &cid) {
            let _ = f.write_all(line.as_bytes());
            let _ = f.write_all(b"\n");
        }
    }
}
