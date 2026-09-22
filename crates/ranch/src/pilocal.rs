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
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ranch_protocol::{ChatMsg, Frame, ModelChoice};
use uuid::Uuid;

use crate::forge::PipeWriter;

/// Monotonic message seq for locally-generated chat rows (per pane the
/// clients only need strictly-increasing; a process-wide counter is the
/// simplest way to guarantee it across panes).
static SEQ: AtomicU64 = AtomicU64::new(1);
fn next_seq() -> i64 {
    SEQ.fetch_add(1, Ordering::Relaxed) as i64
}

/// Epoch ms -> UTC "YYYY-MM-DDTHH:MM:SSZ" (the `created_at` format the
/// clients expect; they slice HH:MM out of [11..16]).
fn iso_utc_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86400);
    let sod = secs.rem_euclid(86400);
    // civil calendar (Howard Hinnant's days-from-civil inverse)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (yoe * 365 + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let yy = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        yy, m, d, sod / 3600, (sod % 3600) / 60, sod % 60
    )
}

/// Wall-clock `created_at` for a row emitted right now. This is the
/// moment the agent sent it locally (the RPC event just arrived), not
/// when any client observed it.
fn now_iso() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    iso_utc_ms(ms)
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
    /// raw fd wrapped as File (ChildStdin lacks FromRawFd). Arc+Mutex:
    /// the prompt/send paths take &self and the reader thread issues its
    /// own read-only RPCs (`get_session_stats` after each turn).
    stdin: Arc<Mutex<Option<Box<dyn std::io::Write + Send>>>>,
    /// Read end of the rpc child's stdout (same).
    #[allow(dead_code)]
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
    /// The pi model currently active in this child, captured from the
    /// `get_state` / `set_model` responses. Displayed in chat-pane UIs.
    model: Arc<Mutex<Option<ModelChoice>>>,
    /// req_id of an in-flight `get_available_models`; the reader thread
    /// echoes it back in the `ModelListOk` it emits.
    model_list_req: Arc<Mutex<Option<String>>>,
    /// req_id of an in-flight `set_model`; echoed in the `error` frame
    /// when the switch fails.
    model_set_req: Arc<Mutex<Option<String>>>,
    /// req_id of an in-flight `compact`; the reader thread echoes it in
    /// the `error` frame when the compaction fails.
    compact_req: Arc<Mutex<Option<String>>>,
    /// Set by `switch_session`: `session_file` holds the switch TARGET,
    /// not yet echoed back by pi. While set, a `get_state` reply naming a
    /// different path is a stale in-flight answer (the spawn-time
    /// get_state racing the switch) and is ignored instead of clobbering
    /// the pin. Cleared once pi reports the pinned path.
    pin_unconfirmed: Arc<AtomicBool>,
}

/// `pi_no_tools = "true"` in ~/.config/ranch/daemon.toml disables all
/// agent tools for locally-backed panes (public-demo hardening: the
/// agent can chat but must not touch the file system or spawn
/// processes — same posture as mule's `no_tools` config).
pub fn no_tools_configured() -> bool {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return false,
    };
    let text = match std::fs::read_to_string(std::path::PathBuf::from(home).join(".config/ranch/daemon.toml")) {
        Ok(t) => t,
        Err(_) => return false,
    };
    for line in text.lines() {
        let line = line.trim();
        // exact key: `pi_no_tools = "true"` (flat daemon.toml format)
        let rest = match line.strip_prefix("pi_no_tools") {
            Some(r) => r.trim_start(),
            None => continue,
        };
        if rest.is_empty() || !rest.starts_with('=') {
            continue; // e.g. `pi_no_tools_foo`
        }
        let v = rest[1..].trim().trim_matches('"');
        return v.eq_ignore_ascii_case("true");
    }
    false
}

impl LocalPi {
    /// Locate the ranch pi extension (agent tools). Shipped in-repo at
    /// `tools/ranch-pi-ext/`; an install copies it to
    /// `~/.local/share/ranch/ranch-pi-ext` or RANCH_EXT_DIR overrides.
    fn ranch_ext_path() -> Option<std::path::PathBuf> {
        if let Ok(dir) = std::env::var("RANCH_EXT_DIR") {
            let p = std::path::PathBuf::from(dir);
            if p.join("index.js").exists() {
                return Some(p);
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            let p =
                std::path::PathBuf::from(home).join(".local/share/ranch/ranch-pi-ext");
            if p.join("index.js").exists() {
                return Some(p);
            }
        }
        // dev: the repo checkout (daemon run via `make run` from the repo)
        let rel = std::path::PathBuf::from("tools/ranch-pi-ext");
        if rel.join("index.js").exists() {
            return Some(rel);
        }
        None
    }

    /// Locate the `pi` binary. systemd-launched daemons run with a minimal
    /// PATH (`/usr/local/bin:/usr/bin`) that usually omits mise's install
    /// dirs, so a bare `Command::new("pi")` fails with
    /// "No such file or directory" even when the user's shell can run `pi`.
    ///
    /// Resolution order:
    ///   1. `pi_bin = "..."` in ~/.config/ranch/daemon.toml (explicit override)
    ///   2. `RANCH_PI_BIN` env var
    ///   3. `~/.local/bin/pi` (mise shim wrapper, always on user PATH)
    ///   4. `~/.local/share/mise/shims/pi`
    ///   5. `pi` (fall back to PATH lookup)
    ///
    /// Returns a path string to hand to `Command::new`.
    fn pi_bin() -> String {
        // 1. daemon.toml `pi_bin = "..."`
        if let Ok(home) = std::env::var("HOME") {
            if let Ok(text) =
                std::fs::read_to_string(
                    std::path::PathBuf::from(&home).join(".config/ranch/daemon.toml"),
                )
            {
                for line in text.lines() {
                    let line = line.trim();
                    let rest = match line.strip_prefix("pi_bin") {
                        Some(r) => r.trim_start(),
                        None => continue,
                    };
                    if rest.is_empty() || !rest.starts_with('=') {
                        continue; // e.g. `pi_binary`
                    }
                    let v = rest[1..].trim().trim_matches('"').to_string();
                    if !v.is_empty() {
                        return v;
                    }
                }
            }
        }
        // 2. env override
        if let Ok(p) = std::env::var("RANCH_PI_BIN") {
            if !p.is_empty() {
                return p;
            }
        }
        // 3 + 4. well-known mise locations
        if let Ok(home) = std::env::var("HOME") {
            for cand in [
                format!("{home}/.local/bin/pi"),
                format!("{home}/.local/share/mise/shims/pi"),
            ] {
                if std::path::Path::new(&cand).exists() {
                    return cand;
                }
            }
        }
        // 5. rely on PATH
        "pi".to_string()
    }

    /// Spawn `pi --mode rpc` in `cwd`. Registers itself in `panes`;
    /// the stdout reader thread starts immediately and emits
    /// Chat/Meta frames into `pipe` (session left blank — the main
    /// loop's pre-match fills it and broadcasts).
    pub fn spawn(
        pane: Uuid,
        cwd: &str,
        no_tools: bool,
        pipe: PipeWriter,
        panes: &mut BTreeMap<Uuid, Arc<LocalPi>>,
    ) -> Result<(), String> {
        let bin = Self::pi_bin();
        let mut child = Command::new(&bin);
        child.arg("--mode").arg("rpc");
        if no_tools {
            child.arg("--no-tools");
        }
        // agent tools (Phase A): when the loopback control API is up,
        // load the ranch extension and hand the child its credentials
        // (token + port exported by control_api::spawn; the pane id is
        // per-child). The extension registers ranch_spawn et al.
        let control = (
            std::env::var("RANCH_CONTROL_TOKEN").ok(),
            std::env::var("RANCH_CONTROL_PORT").ok(),
        );
        if let (Some(_tok), Some(_port)) = &control {
            if let Some(ext) = Self::ranch_ext_path() {
                // -e resolves against the CHILD's cwd — canonicalize so
                // the extension loads regardless of spawn dir
                let ext = std::fs::canonicalize(&ext)
                    .unwrap_or(ext);
                child.arg("-e").arg(&ext);
            }
        }
        if let (Some(tok), Some(port)) = &control {
            child.env("RANCH_CONTROL_TOKEN", tok);
            child.env("RANCH_CONTROL_PORT", port);
            child.env("RANCH_CONTROL_PANE", pane.to_string());
        }
        let mut child = child
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
            stdin: Arc::new(Mutex::new(Some(Box::new(stdin)))),
            stdout: Mutex::new(Some(Box::new(stdout))),
            raw_fds: (Some(stdin_fd), Some(stdout_fd)),
            stop: Arc::new(AtomicBool::new(false)),
            session_file: Arc::new(Mutex::new(None)),
            model: Arc::new(Mutex::new(None)),
            model_list_req: Arc::new(Mutex::new(None)),
            model_set_req: Arc::new(Mutex::new(None)),
            compact_req: Arc::new(Mutex::new(None)),
            pin_unconfirmed: Arc::new(AtomicBool::new(false)),
        });
        panes.insert(pane, lp.clone());
        lp.start_reader(pipe);

        // ask pi for its session file path + current model (responses
        // are captured in the reader); harmless if they arrive before/
        // after the first prompt
        let _ = lp.send_rpc(&serde_json::json!({"type": "get_state"}));
        // initial context-window readout
        let _ = lp.send_rpc(&serde_json::json!({"type": "get_session_stats"}));
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
            stdin: Arc::new(Mutex::new(Some(Box::new(unsafe {
                std::fs::File::from_raw_fd(stdin_fd)
            })))),

            stdout: Mutex::new(Some(Box::new(unsafe {
                std::fs::File::from_raw_fd(stdout_fd)
            }))),
            raw_fds: (Some(stdin_fd), Some(stdout_fd)),
            stop: Arc::new(AtomicBool::new(false)),
            session_file: Arc::new(Mutex::new(None)),
            model: Arc::new(Mutex::new(None)),
            model_list_req: Arc::new(Mutex::new(None)),
            model_set_req: Arc::new(Mutex::new(None)),
            compact_req: Arc::new(Mutex::new(None)),
            pin_unconfirmed: Arc::new(AtomicBool::new(false)),
        });
        panes.insert(pane, lp.clone());
        lp.start_reader(pipe);
        // re-capture the session file + model AND resync the conversation rows
        // (the inheriting daemon's chat buffer starts empty)
        let _ = lp.send_rpc(&serde_json::json!({"type": "get_state"}));
        let _ = lp.send_rpc(&serde_json::json!({"type": "get_messages"}));
        // initial context-window readout
        let _ = lp.send_rpc(&serde_json::json!({"type": "get_session_stats"}));
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
        let model = self.model.clone();
        let model_list_req = self.model_list_req.clone();
        let model_set_req = self.model_set_req.clone();
        let compact_req = self.compact_req.clone();
        let pin_unconfirmed = self.pin_unconfirmed.clone();
        let stdin = self.stdin.clone();
        std::thread::spawn(move || {
            run_pi_reader(
                dup,
                t_pane,
                session_file,
                stop,
                model,
                model_list_req,
                model_set_req,
                compact_req,
                pin_unconfirmed,
                stdin,
                pipe,
            );
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

    /// Current model display name (None when unknown).
    #[allow(dead_code)]
    pub fn model_display(&self) -> Option<String> {
        self.model.lock().ok()?.clone().map(|m| m.name)
    }

    /// Ask pi for its configured model list; the reader thread emits a
    /// `ModelListOk` (echoing `req_id`) when the response arrives.
    #[allow(dead_code)]
    pub fn request_model_list(&self, req_id: &str) -> Result<(), String> {
        *self.model_list_req.lock().unwrap() = Some(req_id.to_string());
        self.send_rpc(&serde_json::json!({"type": "get_available_models"}))
    }

    /// Switch the active model. The `set_model` response (reader thread)
    /// confirms with `meta kind="model"` or an `error` frame echoing
    /// `req_id`.
    pub fn set_model(&self, provider: &str, model: &str, req_id: &str) -> Result<(), String> {
        *self.model_set_req.lock().unwrap() = Some(req_id.to_string());
        self.send_rpc(&serde_json::json!({
            "type": "set_model",
            "provider": provider,
            "modelId": model,
        }))
    }

    /// Manually compact the agent's context now (pi `compact` RPC).
    /// The reader thread confirms with `meta kind="context"` on
    /// success or an `error { req_id }` frame on failure.
    pub fn compact(&self, req_id: &str) -> Result<(), String> {
        *self.compact_req.lock().unwrap() = Some(req_id.to_string());
        self.send_rpc(&serde_json::json!({"type": "compact"}))
    }

    /// Ask pi for its context-window usage; the reader thread emits
    /// `meta kind="context"` when the response arrives.
    #[allow(dead_code)]
    pub fn request_stats(&self) -> Result<(), String> {
        self.send_rpc(&serde_json::json!({"type": "get_session_stats"}))
    }

    /// Switch pi to a previously-recorded session file (restore path).
    ///
    /// Also records `path` as the pane's session file immediately and
    /// re-queries `get_state`. Without this, the `get_state` fired at
    /// spawn wins the race: pi answers with the fresh (empty) session it
    /// auto-created, that path lands in `session_file` → `state.json`,
    /// and the NEXT restart points at a file that was never written
    /// (pi lazy-creates session files on first message).
    pub fn switch_session(&self, path: &str) -> Result<(), String> {
        self.send_rpc(&serde_json::json!({
            "type": "switch_session",
            "sessionPath": path,
        }))?;
        if let Ok(mut g) = self.session_file.lock() {
            *g = Some(path.to_string());
            self.pin_unconfirmed.store(true, Ordering::Relaxed);
            STATE_DIRTY.store(true, Ordering::Relaxed);
        }
        // re-read so the recorded path tracks whatever pi actually
        // settled on (and the model label refreshes)
        self.send_rpc(&serde_json::json!({"type": "get_state"}))
    }

    /// Pick the session file to restore into: the recorded path, only
    /// if it exists on disk. A recorded path can be missing because pi
    /// lazy-creates session files on the first message — a pane that was
    /// restored and then never prompted has a path that was never
    /// written. Deliberately NO "newest file for this cwd" fallback:
    /// that file is very likely another live pi's conversation (a
    /// terminal `pi` in the same dir), and switching into it would put
    /// two writers on one JSONL. Fresh beats hijacked.
    pub fn resolve_restore_file(recorded: Option<&str>) -> Option<String> {
        recorded
            .filter(|r| std::path::Path::new(r).is_file())
            .map(str::to_string)
    }


/// Read a pi session JSONL file from disk and parse it into chat messages.
///
/// `switch_session` tells pi to use a session file for future writes but
/// does NOT load historical messages into its in-memory state, so
/// `get_messages` returns empty on a freshly-spawned process.  This
/// function bypasses the RPC entirely and reads the file directly.
pub fn read_session_messages(path: &str) -> Result<Vec<ChatMsg>, String> {
    use std::io::BufRead;

    let file = std::fs::File::open(path)
        .map_err(|e| format!("cannot open session file {path}: {e}"))?;
    let reader = std::io::BufReader::new(file);

    let mut msgs: Vec<ChatMsg> = Vec::new();
    // toolCall id -> row index, so a toolResult row attaches its output
    // to the matching tool row instead of piling on at the end
    let mut tool_rows: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Only process lines that are conversation messages
        if v.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }

        let msg = match v.get("message") {
            Some(m) => m,
            None => continue,
        };

        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let c = msg.get("content");
        let ts = msg
            .get("timestamp")
            .or_else(|| v.get("timestamp"))
            .and_then(|t| t.as_str())
            .map(|s| {
                // "2026-09-02T13:29:33.190Z" -> "2026-09-02T13:29:33Z"
                if s.len() >= 19 {
                    format!("{}Z", &s[..19])
                } else {
                    s.to_string()
                }
            });

        let push = |msgs: &mut Vec<ChatMsg>,
                    role: &str,
                    text: String,
                    tool_name: Option<String>,
                    tool_output: Option<String>,
                    created_at: Option<String>,
                    next_seq: &AtomicU64| {
            msgs.push(ChatMsg {
                seq: next_seq.fetch_add(1, Ordering::Relaxed) as i64,
                role: role.to_string(),
                text,
                tool_name,
                tool_call_id: None,
                tool_output,
                duration_ms: None,
                created_at,
                attachments: None,
            });
        };

        match role {
            "user" => {
                let t = match c {
                    Some(serde_json::Value::String(s)) => s.clone(),
                    Some(serde_json::Value::Array(arr)) => arr
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
                    push(
                        &mut msgs, "user", t, None, None, ts.clone(), &SEQ,
                    );
                }
            }
            "assistant" => {
                if let Some(serde_json::Value::Array(arr)) = c {
                    for blk in arr {
                        match blk.get("type").and_then(|t| t.as_str()) {
                            Some("text") => {
                                if let Some(text) =
                                    blk.get("text").and_then(|t| t.as_str())
                                {
                                    let t = text.trim();
                                    if !t.is_empty() {
                                        push(
                                            &mut msgs, "assistant",
                                            t.to_string(), None, None,
                                            ts.clone(), &SEQ,
                                        );
                                    }
                                }
                            }
                            Some("toolCall") => {
                                let name = blk
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("tool")
                                    .to_string();
                                let call_id = blk
                                    .get("id")
                                    .and_then(|i| i.as_str())
                                    .map(String::from);
                                let idx = msgs.len();
                                push(
                                    &mut msgs, "tool", String::new(),
                                    Some(name), None, ts.clone(), &SEQ,
                                );
                                if let Some(id) = call_id {
                                    tool_rows.insert(id, idx);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            "toolResult" => {
                let out = match c {
                    Some(serde_json::Value::Array(arr)) => arr
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
                let call_id = msg
                    .get("toolCallId")
                    .or_else(|| msg.get("tool_call_id"))
                    .and_then(|i| i.as_str())
                    .map(String::from);
                let idx = call_id.as_ref().and_then(|cid| {
                    tool_rows.get(cid).copied()
                });
                if let Some(idx) = idx {
                    if msgs[idx].tool_output.is_none() {
                        msgs[idx].tool_output = Some(out);
                    }
                } else {
                    let name = msg
                        .get("toolName")
                        .or_else(|| msg.get("tool_name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("tool")
                        .to_string();
                    push(
                        &mut msgs, "tool", String::new(),
                        Some(name), Some(out), ts.clone(), &SEQ,
                    );
                }
            }
            _ => {}
        }
    }

    Ok(msgs)
}

    /// Send a user prompt: write the RPC prompt to pi's stdin first,
    /// then record the user row + flip the working indicator (so a
    /// failed write doesn't leave phantom rows).
    ///
    /// `agent_text` is what pi receives (may include inlined attachment
    /// content); `display_text` is what the user sees in the chat row;
    /// `attachments` are file paths shown as badges on the user row.
    pub fn prompt(&self, pipe: &PipeWriter, agent_text: &str, display_text: &str, attachments: &[String]) -> Result<(), String> {
        let send = || -> Result<(), String> {
            let mut g = self
                .stdin
                .lock()
                .map_err(|_| "pi stdin poisoned".to_string())?;
            let s = g
                .as_mut()
                .ok_or_else(|| "pi stdin already taken".to_string())?;
            let line = serde_json::json!({"type": "prompt", "message": agent_text}).to_string();
            s.write_all(line.as_bytes())
                .and_then(|_| s.write_all(b"\n"))
                .and_then(|_| s.flush())
                .map_err(|e| format!("pi stdin: {e}"))?;
            Ok(())
        };
        if let Err(e) = send() {
            // a failed write emits the error row + clears the indicator
            // instead — otherwise the message vanishes silently on every
            // client (the phone worst of all, where there is no daemon
            // log to check)
            emit_chat(
                pipe,
                self.pane,
                "assistant",
                &format!("⚠ message not sent: {e}"),
                now_iso(),
            );
            return Err(e);
        }
        let att = if attachments.is_empty() { None } else { Some(attachments.to_vec()) };
        emit_chat_with(pipe, self.pane, "user", display_text, att, now_iso());
        write_status(pipe, self.pane, "working");
        Ok(())
    }

    /// Kill the child (pane close). Best-effort. On the inherit path the
    /// pid is signaled directly (no Child handle).
    /// Kill the rpc child. Returns its pid so the caller can reap it
    /// (push onto the daemon's `orphans` list) — dropping a `Child`
    /// without `wait` leaves a zombie for the daemon's lifetime.
    pub fn kill(&self) -> Option<libc::pid_t> {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut guard) = self.child.lock() {
            if let Some(c) = guard.as_mut() {
                let pid = c.id() as libc::pid_t;
                let _ = c.kill();
                *guard = None;
                return Some(pid);
            }
        }
        if let Ok(g) = self.inherited_pid.lock() {
            if let Some(pid) = *g {
                unsafe { libc::kill(pid, libc::SIGKILL) };
                return Some(pid);
            }
        }
        None
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
    model: Arc<Mutex<Option<ModelChoice>>>,
    model_list_req: Arc<Mutex<Option<String>>>,
    model_set_req: Arc<Mutex<Option<String>>>,
    compact_req: Arc<Mutex<Option<String>>>,
    pin_unconfirmed: Arc<AtomicBool>,
    stdin: Arc<Mutex<Option<Box<dyn std::io::Write + Send>>>>,
    pipe: PipeWriter,
) {
    /// Fire a read-only RPC from the reader thread (best-effort; used
    /// for context-window refreshes).
    fn rpc(stdin: &Arc<Mutex<Option<Box<dyn std::io::Write + Send>>>>, cmd: &str) {
        if let Ok(mut g) = stdin.lock() {
            if let Some(s) = g.as_mut() {
                let _ = s.write_all(cmd.as_bytes());
                let _ = s.write_all(b"\n");
                let _ = s.flush();
            }
        }
    }
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
                        if let Ok(mut g) = session_file.lock() {
                            // After a switch_session we pin the target path.
                            // pi answers RPCs in order, but the spawn-time
                            // get_state may still be in flight when the switch
                            // is sent — its late reply carries the FRESH
                            // auto-created path and must not clobber the pin.
                            // Accept a different path only once pi has
                            // confirmed the pinned one at least once (a
                            // genuine later change, e.g. /new).
                            let pinned = g.as_ref().filter(|cur| {
                                pin_unconfirmed.load(Ordering::Relaxed) && cur.as_str() != sf
                            });
                            if pinned.is_some() {
                                eprintln!("ranchd: local pi {t_pane} ignoring stale session file {sf} (pinned to {})", g.as_deref().unwrap_or(""));
                            } else {
                                if g.as_deref() == Some(sf) {
                                    pin_unconfirmed.store(false, Ordering::Relaxed);
                                }
                                if g.as_deref() != Some(sf) {
                                    eprintln!("ranchd: local pi {t_pane} session file: {sf}");
                                    *g = Some(sf.to_string());
                                    STATE_DIRTY.store(true, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                    // capture the active model and tell clients what the
                    // pane is running (meta kind="model" display name)
                    let m = v.pointer("/data/model")
                        .filter(|m| !m.is_null())
                        .and_then(parse_model);
                    if let Some(m) = &m {
                        if let Ok(mut g) = model.lock() {
                            *g = Some(m.clone());
                        }
                        write_model_status(&pipe, t_pane, &m.name);
                    }
                } else if v.get("command").and_then(|c| c.as_str()) == Some("get_available_models") {
                    // model picker request: echo the catalog back (the
                    // pane's own model was last captured via get_state /
                    // set_model)
                    let req_id = model_list_req.lock().ok().and_then(|mut g| g.take()).unwrap_or_default();
                    let mut models = Vec::new();
                    if let Some(arr) = v.pointer("/data/models").and_then(|m| m.as_array()) {
                        for m in arr {
                            if let Some(mc) = parse_model(m) {
                                models.push(mc);
                            }
                        }
                    }
                    let current = model.lock().ok().and_then(|g| g.clone());
                    write_frame(
                        &pipe,
                        &Frame::ModelListOk {
                            id: String::new(),
                            req_id,
                            pane: t_pane.to_string(),
                            current,
                            models,
                        },
                    );
                } else if v.get("command").and_then(|c| c.as_str()) == Some("set_model") {
                    let success = v.get("success").and_then(|s| s.as_bool()).unwrap_or(false);
                    if success {
                        if let Some(m) = v.pointer("/data").and_then(parse_model) {
                            if let Ok(mut g) = model.lock() {
                                *g = Some(m.clone());
                            }
                            write_model_status(&pipe, t_pane, &m.name);
                        }
                    } else {
                        let msg = v
                            .get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("model switch failed")
                            .to_string();
                        eprintln!("ranchd: local pi {t_pane} set_model failed: {msg}");
                        let req_id = model_set_req.lock().ok().and_then(|mut g| g.take());
                        write_frame(
                            &pipe,
                            &Frame::Error {
                                req_id,
                                message: format!("model switch failed: {msg}"),
                            },
                        );
                        write_status(&pipe, t_pane, "idle");
                    }
                } else if v.get("command").and_then(|c| c.as_str()) == Some("compact") {
                    let success = v.get("success").and_then(|s| s.as_bool()).unwrap_or(false);
                    if success {
                        let after = v
                            .pointer("/data/estimatedTokensAfter")
                            .and_then(|a| a.as_i64())
                            .map(|n| format_k_tokens(n))
                            .unwrap_or_default();
                        let note = if after.is_empty() {
                            "compacted".to_string()
                        } else {
                            format!("compacted → {after} est. tokens")
                        };
                        write_context_status(&pipe, t_pane, &note);
                        // refresh the live readout (pi re-reported after
                        // the window shrank)
                        rpc(
                            &stdin,
                            r#"{"type": "get_session_stats"}"#,
                        );
                    } else {
                        let msg = v
                            .get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("compaction failed")
                            .to_string();
                        eprintln!("ranchd: local pi {t_pane} compact failed: {msg}");
                        let req_id = compact_req.lock().ok().and_then(|mut g| g.take());
                        write_frame(
                            &pipe,
                            &Frame::Error {
                                req_id,
                                message: format!("compact failed: {msg}"),
                            },
                        );
                        write_status(&pipe, t_pane, "idle");
                    }
                } else if v.get("command").and_then(|c| c.as_str()) == Some("get_session_stats") {
                    // context-window readout (footer-style numbers)
                    if let Some(cu) = v.pointer("/data/contextUsage") {
                        let tokens = cu.get("tokens").and_then(|x| x.as_i64());
                        let window = cu.get("contextWindow").and_then(|x| x.as_i64());
                        let pct = cu.get("percent").and_then(|x| x.as_f64());
                        let status = match (tokens, window, pct) {
                            (Some(t), Some(w), Some(p)) => {
                                format!("ctx {p:.0}% · {}/{}", format_k_tokens(t), format_k_tokens(w))
                            }
                            (Some(t), _, _) => format!("ctx ~{} est.", format_k_tokens(t)),
                            _ => String::new(),
                        };
                        if !status.is_empty() {
                            write_context_status(&pipe, t_pane, &status);
                        }
                    }
                } else if v.get("command").and_then(|c| c.as_str()) == Some("get_messages") {
                    // hot-upgrade resync: rebuild the pane's chat rows from
                    // pi's own persisted conversation (the inheriting
                    // daemon's row buffer starts empty). Rows are emitted in
                    // conversation order — text and tool rows interleaved as
                    // pi recorded them, NOT all-text-then-all-tools.
                    let mut msgs: Vec<ChatMsg> = Vec::new();
                    // toolCall id -> row index, so a toolResult row attaches
                    // its output to the right tool row instead of piling on
                    // at the end
                    let mut tool_rows: Vec<(String, usize)> = Vec::new(); // (call_id, msg idx)
                    if let Some(arr) = v.pointer("/data/messages").and_then(|m| m.as_array()) {
                        for m in arr {
                            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
                            let c = m.get("content");
                            // pi records the agent's own send time (epoch ms)
                            let ts = m
                                .get("timestamp")
                                .and_then(|t| t.as_i64())
                                .map(iso_utc_ms);
                            let push = |msgs: &mut Vec<ChatMsg>,
                                            role: &str,
                                            text: String,
                                            tool_name: Option<String>,
                                            tool_output: Option<String>|
                            {
                                msgs.push(ChatMsg {
                                    seq: next_seq(),
                                    role: role.to_string(),
                                    text,
                                    tool_name,
                                    tool_call_id: None,
                                    tool_output,
                                    duration_ms: None,
                                    created_at: ts.clone(),
                                    attachments: None,
                                });
                            };
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
                                        push(&mut msgs, "user", t, None, None);
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
                                                            push(
                                                                &mut msgs,
                                                                "assistant",
                                                                t.to_string(),
                                                                None,
                                                                None,
                                                            );
                                                        }
                                                    }
                                                }
                                                Some("toolCall") => {
                                                    let name = blk
                                                        .get("name")
                                                        .and_then(|n| n.as_str())
                                                        .unwrap_or("tool")
                                                        .to_string();
                                                    let call_id = blk
                                                        .get("id")
                                                        .and_then(|i| i.as_str())
                                                        .map(String::from);
                                                    let idx = msgs.len();
                                                    push(
                                                        &mut msgs,
                                                        "tool",
                                                        String::new(),
                                                        Some(name),
                                                        None,
                                                    );
                                                    if let Some(id) = call_id {
                                                        tool_rows.push((id, idx));
                                                    }
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
                                    // fill the output into the matching tool
                                    // row (by call id), else the last
                                    // unfinished tool row; else fall back to a
                                    // standalone tool row
                                    let call_id = m
                                        .get("toolCallId")
                                        .or_else(|| m.get("tool_call_id"))
                                        .and_then(|i| i.as_str())
                                        .map(String::from);
                                    let idx = call_id.as_deref().and_then(|cid| {
                                        tool_rows
                                            .iter()
                                            .rev()
                                            .find(|(k, _)| k == cid)
                                            .map(|(_, i)| *i)
                                    });
                                    if let Some(idx) = idx {
                                        if msgs[idx].tool_output.is_none() {
                                            msgs[idx].tool_output = Some(out);
                                        }
                                    } else {
                                        let name = m
                                            .get("toolName")
                                            .or_else(|| m.get("tool_name"))
                                            .and_then(|n| n.as_str())
                                            .unwrap_or("tool")
                                            .to_string();
                                        push(&mut msgs, "tool", String::new(), Some(name), Some(out));
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    if !msgs.is_empty() {
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
                        let stop = msg
                            .get("stopReason")
                            .and_then(|s| s.as_str())
                            .unwrap_or("");
                        if !trimmed.is_empty() {
                            // stamp with the agent's own timestamp when
                            // it carried one, else the arrival moment
                            let ts = msg
                                .get("timestamp")
                                .and_then(|t| t.as_i64())
                                .map(iso_utc_ms)
                                .unwrap_or_else(now_iso);
                            emit_chat(&pipe, t_pane, "assistant", trimmed, ts);
                        } else if stop == "error" {
                            // a failed provider call ends the message with
                            // stopReason "error" and no text — without this
                            // row the turn just goes quiet and every client
                            // (TUI + both mobile apps) shows nothing
                            emit_chat(
                                &pipe,
                                t_pane,
                                "assistant",
                                "⚠ the model returned an error (no message)",
                                now_iso(),
                            );
                            write_status(&pipe, t_pane, "idle");
                        }
                    }
                }
            }
            // pi never emits a `type:"error"` RPC event — turn failures
            // surface through the retry events below (and stopReason
            // "error" on message_end). These ⚠ rows are how agent errors
            // reach every client, mobile included.
            "auto_retry_start" => {
                // transient provider failure: pi retries on its own, so
                // the turn is still live — keep the working indicator up
                let attempt = v.get("attempt").and_then(|a| a.as_i64()).unwrap_or(0);
                let max = v.get("maxAttempts").and_then(|a| a.as_i64()).unwrap_or(0);
                let msg = v
                    .get("errorMessage")
                    .and_then(|m| m.as_str())
                    .unwrap_or("transient error");
                let attempt_txt = if max > 0 {
                    format!(" (attempt {attempt}/{max})")
                } else {
                    String::new()
                };
                emit_chat(
                    &pipe,
                    t_pane,
                    "assistant",
                    &format!("⚠ retrying{attempt_txt}: {msg}"),
                    now_iso(),
                );
            }
            "auto_retry_end" => {
                // retries exhausted: success:false + finalError is THE
                // failure signal — emit the row and flip to idle (if a
                // turn_end follows, the duplicate idle is a no-op)
                if v.get("success").and_then(|s| s.as_bool()) == Some(false) {
                    let msg = v
                        .get("finalError")
                        .and_then(|m| m.as_str())
                        .unwrap_or("agent failed");
                    emit_chat(&pipe, t_pane, "assistant", &format!("⚠ {msg}"), now_iso());
                    write_status(&pipe, t_pane, "idle");
                }
            }
            "extension_error" => {
                let msg = v
                    .get("error")
                    .and_then(|m| m.as_str())
                    .unwrap_or("extension error");
                emit_chat(
                    &pipe,
                    t_pane,
                    "assistant",
                    &format!("⚠ extension error: {msg}"),
                    now_iso(),
                );
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
                    emit_tool(&pipe, t_pane, &name, dur, &out, now_iso());
                }
            }
            "turn_end" | "agent_end" => {
                write_status(&pipe, t_pane, "idle");
                // the turn may have grown the context — refresh the
                // readout (reader thread fires the RPC itself)
                rpc(
                    &stdin,
                    r#"{"type": "get_session_stats"}"#,
                );
            }
            "error" => {
                let msg = v
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("pi error");
                emit_chat(&pipe, t_pane, "assistant", &format!("⚠ {msg}"), now_iso());
                write_status(&pipe, t_pane, "idle");
            }
            _ => {}
        }
    }
    // stdout closed: pi is gone
    eprintln!("ranchd: local pi pane {t_pane} exited");
}

fn emit_chat(pipe: &PipeWriter, pane: Uuid, role: &str, text: &str, created_at: String) {
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
                created_at: Some(created_at),
                attachments: None,
            }],
            reset: false,
        },
    );
}

/// Like `emit_chat` but carries attachment file paths on the row.
fn emit_chat_with(
    pipe: &PipeWriter,
    pane: Uuid,
    role: &str,
    text: &str,
    attachments: Option<Vec<String>>,
    created_at: String,
) {
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
                created_at: Some(created_at),
                attachments,
            }],
            reset: false,
        },
    );
}

fn emit_tool(pipe: &PipeWriter, pane: Uuid, name: &str, dur_ms: i64, out: &str, created_at: String) {
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
                created_at: Some(created_at),
                attachments: None,
            }],
            reset: false,
        },
    );
}

/// Parse a pi RPC `Model` object into a `ModelChoice` (name falls
/// back to the model id).
fn parse_model(m: &serde_json::Value) -> Option<ModelChoice> {
    let id = m.get("id")?.as_str()?.to_string();
    let name = m
        .get("name")
        .and_then(|n| n.as_str())
        .filter(|n| !n.is_empty())
        .map(String::from)
        .unwrap_or_else(|| id.clone());
    let provider = m
        .get("provider")
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .to_string();
    Some(ModelChoice { provider, id, name })
}

/// Out-of-band "the pane's model is now X" (kind="model"; the daemon's
/// pre-match stamps the session and broadcasts).
fn write_model_status(pipe: &PipeWriter, pane: Uuid, display: &str) {
    write_frame(
        pipe,
        &Frame::Meta {
            session: String::new(),
            pane: Some(pane.to_string()),
            kind: "model".into(),
            status: Some(display.to_string()),
        },
    );
}

/// Out-of-band context-window readout (kind="context").
fn write_context_status(pipe: &PipeWriter, pane: Uuid, status: &str) {
    write_frame(
        pipe,
        &Frame::Meta {
            session: String::new(),
            pane: Some(pane.to_string()),
            kind: "context".into(),
            status: Some(status.to_string()),
        },
    );
}

/// Human-friendly token count (12345 → "12k").
fn format_k_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1000 {
        format!("{}k", n / 1000)
    } else {
        n.to_string()
    }
}

fn write_status(pipe: &PipeWriter, pane: Uuid, status: &str) {
    write_frame(
        pipe,
        &Frame::Meta {
            session: String::new(),
            pane: Some(pane.to_string()),
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
