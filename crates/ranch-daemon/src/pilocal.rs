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
//!
//! Hot upgrade (Tier 2): the rpc child SURVIVES a daemon execve — its
//! pipes are fd-inherited by the new daemon (`from_inherited`), so agent
//! conversations continue with zero downtime (no respawn, no
//! switch_session replay).

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
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

/// Set by the reader thread when pi's session file is captured — the
/// daemon's poll loop watches this and re-persists state.json so the
/// recorded pi_session_file stays current for restore.
pub static STATE_DIRTY: AtomicBool = AtomicBool::new(false);

/// One local pi agent: the child process + its bookkeeping.
pub struct LocalPi {
    pub pane: Uuid,
    pub cwd: String,
    /// None on the inherit path: the child was spawned by the previous
    /// daemon generation; we still own its reaping, tracked via
    /// `inherited_pid`.
    child: Mutex<Option<Child>>,
    inherited_pid: Mutex<Option<libc::pid_t>>,
    /// Write end of the rpc child's stdin. On the inherit path this is a
    /// raw fd wrapped as File (ChildStdin lacks FromRawFd). Mutex: the
    /// prompt/send paths take &self.
    stdin: Mutex<Option<Box<dyn std::io::Write + Send>>>,
    /// Read end of the rpc child's stdout (same).
    stdout: Mutex<Option<Box<dyn std::io::Read + Send>>>,
    /// Raw fds backing stdin/stdout — kept separately so the hot-upgrade
    /// path can hand them to the next generation (trait objects can't
    /// surface as_raw_fd).
    raw_fds: (Option<RawFd>, Option<RawFd>),
    stop: Arc<AtomicBool>,
    /// pi's own session file, captured from the `get_state` response so a
    /// respawned pi can `switch_session` back into the same conversation
    /// (session persistence across daemon restarts).
    pub session_file: Arc<Mutex<Option<String>>>,
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
        let stdin_fd = stdin.as_raw_fd();
        let stdout_fd = stdout.as_raw_fd();

        let lp = Arc::new(LocalPi {
            pane,
            cwd: cwd.to_string(),
            child: Mutex::new(Some(child)),
            inherited_pid: Mutex::new(None),
            stdin: Mutex::new(Some(Box::new(stdin))),
            stdout: Mutex::new(Some(Box::new(stdout))),
            raw_fds: (Some(stdin_fd), Some(stdout_fd)),
            stop: Arc::new(AtomicBool::new(false)),
            session_file: Arc::new(Mutex::new(None)),
        });
        panes.insert(pane, lp.clone());
        lp.start_reader(pipe);

        // ask pi for its session file path (response is captured in the
        // reader); harmless if it arrives before/after the first prompt
        let _ = lp.send_rpc(&serde_json::json!({"type": "get_state"}));
        Ok(())
    }

    /// Raw fds of the rpc child's stdin/stdout pipes (hot-upgrade: the
    /// inheriting daemon needs these to rebuild LocalPi around the SAME
    /// child process). The dup'd fd is CLOEXEC-cleaned by the caller.
    pub fn raw_stdin(&self) -> Option<RawFd> {
        self.raw_fds.0
    }
    pub fn raw_stdout(&self) -> Option<RawFd> {
        self.raw_fds.1
    }

    /// pid of the rpc child (inherited or spawned) for /proc lookups.
    pub fn child_pid(&self) -> Option<libc::pid_t> {
        if let Ok(g) = self.child.lock() {
            if let Some(c) = g.as_ref() {
                return Some(c.id() as libc::pid_t);
            }
        }
        if let Ok(g) = self.inherited_pid.lock() {
            return *g;
        }
        None
    }
    /// Rebuild a LocalPi around an EXISTING rpc child (hot-upgrade
    /// inherit path): the child survived the exec, its pipes were passed
    /// through; only the reader thread must be restarted.
    pub fn from_inherited(
        pane: Uuid,
        cwd: &str,
        stdin_fd: RawFd,
        stdout_fd: RawFd,
        child_pid: Option<libc::pid_t>,
        pipe: PipeWriter,
        panes: &mut BTreeMap<Uuid, Arc<LocalPi>>,
    ) -> Result<(), String> {
        use std::os::fd::FromRawFd;
        // take ownership of the passed fds (they were CLOEXEC-cleared by
        // the previous generation; we re-add CLOEXEC for hygiene)
        for fd in [stdin_fd, stdout_fd] {
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags >= 0 {
                    libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                }
            }
        }
        let lp = Arc::new(LocalPi {
            pane,
            cwd: cwd.to_string(),
            child: Mutex::new(None),
            inherited_pid: Mutex::new(child_pid),
            // ChildStdin/Stdout don't implement FromRawFd — use owned
            // Files (Read/Write impls are equivalent for our use)
            stdin: Mutex::new(Some(Box::new(unsafe {
                std::fs::File::from_raw_fd(stdin_fd)
            }))),
            stdout: Mutex::new(Some(Box::new(unsafe {
                std::fs::File::from_raw_fd(stdout_fd)
            }))),
            raw_fds: (Some(stdin_fd), Some(stdout_fd)),
            stop: Arc::new(AtomicBool::new(false)),
            session_file: Arc::new(Mutex::new(None)),
        });
        panes.insert(pane, lp.clone());
        lp.start_reader(pipe);
        // re-capture the session file AND resync the conversation rows
        // (the inheriting daemon's chat buffer starts empty)
        let _ = lp.send_rpc(&serde_json::json!({"type": "get_state"}));
        let _ = lp.send_rpc(&serde_json::json!({"type": "get_messages"}));
        Ok(())
    }

    fn start_reader(&self, pipe: PipeWriter) {
        let Some(fd) = self.raw_fds.1 else {
            return;
        };
        // owned dup for the thread (the original stays for kill/rebuild)
        let dup_fd = unsafe { libc::dup(fd) };
        let dup: Box<dyn std::io::Read + Send> =
            Box::new(unsafe { std::fs::File::from_raw_fd(dup_fd) });
        let t_pane = self.pane;
        let session_file = self.session_file.clone();
        let stop = self.stop.clone();
        std::thread::spawn(move || {
            run_pi_reader(dup, t_pane, session_file, stop, pipe);
        });
    }

    /// Send a raw RPC command to pi's stdin (used for `get_state` after
    /// spawn and `switch_session` on restore).
    pub fn send_rpc(&self, cmd: &serde_json::Value) -> Result<(), String> {
        let mut g = self
            .stdin
            .lock()
            .map_err(|_| "pi stdin poisoned".to_string())?;
        let s = g
            .as_mut()
            .ok_or_else(|| "pi stdin already taken".to_string())?;
        let line = cmd.to_string();
        s.write_all(line.as_bytes())
            .and_then(|_| s.write_all(b"\n"))
            .and_then(|_| s.flush())
            .map_err(|e| format!("pi stdin: {e}"))
    }

    /// Switch pi to a previously-recorded session file (restore path).
    pub fn switch_session(&self, path: &str) -> Result<(), String> {
        self.send_rpc(&serde_json::json!({
            "type": "switch_session",
            "sessionPath": path,
        }))
    }

    /// Send a user prompt: write the RPC prompt to pi's stdin first,
    /// then record the user row + flip the working indicator (so a
    /// failed write doesn't leave phantom rows).
    pub fn prompt(&self, pipe: &PipeWriter, text: &str) -> Result<(), String> {
        let mut g = self
            .stdin
            .lock()
            .map_err(|_| "pi stdin poisoned".to_string())?;
        let s = g
            .as_mut()
            .ok_or_else(|| "pi stdin already taken".to_string())?;
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

    /// Kill the child (pane close). Best-effort. On the inherit path the
    /// pid is signaled directly (no Child handle).
    pub fn kill(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut guard) = self.child.lock() {
            if let Some(c) = guard.as_mut() {
                let _ = c.kill();
                *guard = None;
                return;
            }
        }
        if let Ok(g) = self.inherited_pid.lock() {
            if let Some(pid) = *g {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }
}

/// The pi stdout reader: one RPC event line at a time, mapped to chat
/// rows / agent status emitted into `pipe`. Runs on a dedicated thread
/// (restartable — killed by exec on hot upgrade, restarted by the
/// inheriting daemon).
fn run_pi_reader(
    stdout: Box<dyn std::io::Read + Send>,
    t_pane: Uuid,
    session_file: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    pipe: PipeWriter,
) {
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
            // capture pi's session file path (get_state response) so
            // a later daemon restart can switch_session back into
            // this very conversation
            "response" => {
                if v.get("command").and_then(|c| c.as_str()) == Some("get_state") {
                    if let Some(sf) = v.pointer("/data/sessionFile").and_then(|s| s.as_str()) {
                        eprintln!("ranchd: local pi {t_pane} session file: {sf}");
                        if let Ok(mut g) = session_file.lock() {
                            *g = Some(sf.to_string());
                            STATE_DIRTY.store(true, Ordering::Relaxed);
                        }
                    }
                } else if v.get("command").and_then(|c| c.as_str()) == Some("get_messages") {
                    // hot-upgrade resync: rebuild the pane's chat rows from
                    // pi's own persisted conversation (the inheriting
                    // daemon's row buffer starts empty)
                    let mut rows: Vec<(String, String)> = Vec::new(); // (role, text)
                    let mut tools: Vec<(String, String)> = Vec::new(); // (toolName, output)
                    if let Some(msgs) = v.pointer("/data/messages").and_then(|m| m.as_array()) {
                        for m in msgs {
                            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
                            let c = m.get("content");
                            match role {
                                "user" => {
                                    let t = match c {
                                        Some(serde_json::Value::String(s)) => s.clone(),
                                        Some(serde_json::Value::Array(b)) => b
                                            .iter()
                                            .filter(|x| {
                                                x.get("type").and_then(|t| t.as_str())
                                                    == Some("text")
                                            })
                                            .filter_map(|x| x.get("text").and_then(|t| t.as_str()))
                                            .collect::<Vec<_>>()
                                            .join(""),
                                        _ => String::new(),
                                    };
                                    let t = t.trim().to_string();
                                    if !t.is_empty() {
                                        rows.push(("user".into(), t));
                                    }
                                }
                                "assistant" => {
                                    if let Some(serde_json::Value::Array(b)) = c {
                                        for blk in b {
                                            match blk.get("type").and_then(|t| t.as_str()) {
                                                Some("text") => {
                                                    if let Some(t) =
                                                        blk.get("text").and_then(|t| t.as_str())
                                                    {
                                                        let t = t.trim();
                                                        if !t.is_empty() {
                                                            rows.push((
                                                                "assistant".into(),
                                                                t.to_string(),
                                                            ));
                                                        }
                                                    }
                                                }
                                                Some("toolCall") => {
                                                    let name = blk
                                                        .get("name")
                                                        .and_then(|n| n.as_str())
                                                        .unwrap_or("tool");
                                                    tools.push((name.to_string(), String::new()));
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                }
                                "toolResult" => {
                                    let out = match c {
                                        Some(serde_json::Value::Array(b)) => b
                                            .iter()
                                            .filter(|x| {
                                                x.get("type").and_then(|t| t.as_str())
                                                    == Some("text")
                                            })
                                            .filter_map(|x| x.get("text").and_then(|t| t.as_str()))
                                            .collect::<Vec<_>>()
                                            .join(""),
                                        _ => String::new(),
                                    };
                                    if let Some(t) = tools.last_mut() {
                                        t.1 = out;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    if !rows.is_empty() || !tools.is_empty() {
                        let mut msgs: Vec<ChatMsg> = Vec::new();
                        for (role, text) in rows {
                            msgs.push(ChatMsg {
                                seq: next_seq(),
                                role,
                                text,
                                tool_name: None,
                                tool_call_id: None,
                                tool_output: None,
                                duration_ms: None,
                                created_at: None,
                            });
                        }
                        for (name, out) in tools {
                            msgs.push(ChatMsg {
                                seq: next_seq(),
                                role: "tool".into(),
                                text: String::new(),
                                tool_name: Some(name),
                                tool_call_id: None,
                                tool_output: Some(out),
                                duration_ms: None,
                                created_at: None,
                            });
                        }
                        eprintln!(
                            "ranchd: local pi {t_pane} resync: {} rows from get_messages",
                            msgs.len()
                        );
                        write_frame(
                            &pipe,
                            &Frame::Chat {
                                id: String::new(),
                                session: String::new(),
                                pane: t_pane.to_string(),
                                msgs,
                                reset: true,
                            },
                        );
                    }
                }
            }
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
                                        if b.get("type").and_then(|t| t.as_str()) == Some("text") {
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
                    let out = v.get("result").map(|r| r.to_string()).unwrap_or_default();
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
