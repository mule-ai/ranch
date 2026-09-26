//! Agent tools (Phase A): agents running in ranch panes can spawn,
//! steer, observe, and close other agent panes.
//!
//! Everything here is daemon-authoritative: the tool executes in the
//! daemon's [`Daemon::handle_frame`] path whether the request arrived
//! from a local-pi child (loopback control API, `control_api` module)
//! or a remote forge agent (forge bridge — F3a, later phase).
//!
//! Ownership: a pane may only steer/read/close panes recorded in its
//! spawn registry (humans are unrestricted — their frames skip the
//! ownership check).

use std::collections::BTreeMap;
use std::time::Instant;
use uuid::Uuid;

use ranch_protocol::{ChatMsg, Frame};

/// How long an unapproved `ask`-policy spawn request lives before the
/// caller gets `AgentDone { outcome: "denied" }`.
pub const SPAWN_REQUEST_TTL: std::time::Duration = std::time::Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SpawnPolicy {
    Allow,
    Ask,
    Deny,
}

impl SpawnPolicy {
    /// Parse daemon.toml `[agents] spawn_policy`. Unknown values fall
    /// back to Allow (log at the call site if it matters).
    pub fn parse(s: &str) -> SpawnPolicy {
        match s {
            "ask" => SpawnPolicy::Ask,
            "deny" => SpawnPolicy::Deny,
            _ => SpawnPolicy::Allow,
        }
    }

    /// Read `[agents] spawn_policy` from ~/.config/ranch/daemon.toml.
    /// The file uses flat `key = "value"` lines (same parser shape as
    /// relay.rs/forge.rs); a `[agents]` section header is skipped over
    /// by the prefix matching, so a bare `spawn_policy` key works at
    /// any position. Absent file/key = Allow.
    pub fn load() -> SpawnPolicy {
        let Ok(home) = std::env::var("HOME") else {
            return SpawnPolicy::Allow;
        };
        let Ok(text) = std::fs::read_to_string(
            std::path::PathBuf::from(home).join(".config/ranch/daemon.toml"),
        ) else {
            return SpawnPolicy::Allow;
        };
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("spawn_policy") {
                if let Some(rest) = rest.trim_start().strip_prefix('=') {
                    return SpawnPolicy::parse(rest.trim().trim_matches('"'));
                }
            }
        }
        SpawnPolicy::Allow
    }
}

/// One spawned pane: who spawned it, and whether a completion callback
/// is pending.
#[derive(Debug, Clone)]
pub struct SpawnRecord {
    pub spawn_id: String,
    /// The pane that spawned it (nil = human-initiated — humans may
    /// close/steer anything, so the ownership check treats nil as
    /// "human").
    pub caller_pane: Uuid,
    #[allow(dead_code)] // reserved: TTL diagnostics in the ask flow
    pub created_at: Instant,
    pub callback: bool,
    /// The AgentDone for this spawn already fired (first working->idle
    /// transition only — later turns are the agent's own business).
    pub callback_fired: bool,
}

/// Cap on agent-spawned panes per daemon (simple runaway guard).
pub const MAX_SPAWNED_PANES: usize = 32;

/// The daemon's spawn registry.
#[derive(Default)]
pub struct SpawnRegistry {
    /// spawned pane id -> record
    pub by_pane: BTreeMap<Uuid, SpawnRecord>,
    /// spawn_id -> pane id (callback + approval resolution)
    pub by_spawn: BTreeMap<String, Uuid>,
    /// policy=ask: spawn_id -> (record, request broadcast time)
    pub pending: BTreeMap<String, (SpawnRecord, Instant)>,
}

impl SpawnRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_by_pane(&self, pane: Uuid) -> Option<&SpawnRecord> {
        self.by_pane.get(&pane)
    }

    pub fn get_by_pane_mut(&mut self, pane: Uuid) -> Option<&mut SpawnRecord> {
        self.by_pane.get_mut(&pane)
    }

    /// May `caller` operate on `target`? `caller == nil` = human.
    pub fn authorized(&self, caller: Uuid, target: Uuid) -> bool {
        if caller.is_nil() {
            return true; // humans are unrestricted
        }
        // steering yourself is always fine
        if caller == target {
            return true;
        }
        self.by_pane
            .get(&target)
            .is_some_and(|r| r.caller_pane == caller)
    }

    /// Register a spawned pane. Returns Err when the runaway cap is hit.
    pub fn register(&mut self, pane: Uuid, record: SpawnRecord) -> Result<(), String> {
        if self.by_pane.len() >= MAX_SPAWNED_PANES {
            return Err(format!(
                "spawn cap reached ({MAX_SPAWNED_PANES} agent-spawned panes); close some first"
            ));
        }
        self.by_spawn.insert(record.spawn_id.clone(), pane);
        self.by_pane.insert(pane, record);
        Ok(())
    }

    pub fn remove_pane(&mut self, pane: Uuid) -> Option<SpawnRecord> {
        let rec = self.by_pane.remove(&pane)?;
        self.by_spawn.remove(&rec.spawn_id);
        // drop any pending approval for it too
        self.pending.remove(&rec.spawn_id);
        Some(rec)
    }

    /// Expire stale `ask` requests (called from the tick loop). Returns
    /// the spawn ids that timed out (caller sends AgentDone denied).
    pub fn expire_pending(&mut self) -> Vec<(String, Uuid)> {
        let mut expired = Vec::new();
        let ids: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, (_, t))| t.elapsed() > SPAWN_REQUEST_TTL)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some((rec, _)) = self.pending.remove(&id) {
                expired.push((id, rec.caller_pane));
            }
        }
        expired
    }
}

/// Build the system chat row rendered in the caller's pane when a
/// spawned agent completes.
pub fn agent_done_row(spawn: &SpawnRecord, pane: Uuid, outcome: &str, last: Option<&ChatMsg>) -> ChatMsg {
    let label = match outcome {
        "completed" => "finished",
        "failed" => "failed",
        "closed" => "was closed",
        "denied" => "was denied",
        "timeout" => "timed out",
        other => other,
    };
    let tail = last
        .filter(|_| matches!(outcome, "completed" | "failed"))
        .map(|m| {
            let mut t = m.text.clone();
            t.truncate(200);
            format!(" — {}", t.replace('\n', " "))
        })
        .unwrap_or_default();
    ChatMsg {
        seq: next_agent_seq(),
        role: "system".into(),
        text: format!("sub-agent pane {pane} {label}{tail}"),
        tool_name: None,
        // spawn id rides in tool_call_id so clients can correlate
        tool_call_id: Some(spawn.spawn_id.clone()),
        tool_output: None,
        tool_args: None,
        duration_ms: None,
        created_at: Some(now_iso()),
        attachments: None,
        image_refs: None,
    }
}

// ---- small helpers (mirror pilocal's; kept local to avoid cycles) ----

static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_agent_seq() -> i64 {
    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as i64
}

fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    iso_from_unix(secs)
}

/// RFC3339-ish timestamp from unix seconds (same calendar math as
/// daemon.rs `civil_from_unix`).
pub fn iso_from_unix(secs: i64) -> String {
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Days-from-civil inverse (Howard Hinnant), shared local copy.
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    );
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d, h, mi, s)
}

/// Serialize the registry into state.json (Tier-1 restore keeps the
/// ownership + callback relationships).
pub fn registry_value(reg: &SpawnRegistry) -> serde_json::Value {
    serde_json::json!({
        "spawned": reg.by_pane.iter().map(|(pane, r)| serde_json::json!({
            "pane": pane.to_string(),
            "spawn_id": r.spawn_id,
            "caller_pane": r.caller_pane.to_string(),
            "callback": r.callback,
        })).collect::<Vec<_>>(),
    })
}

/// Rebuild the registry from state.json (missing key = empty).
pub fn registry_from_value(v: &serde_json::Value) -> SpawnRegistry {
    let mut reg = SpawnRegistry::new();
    if let Some(items) = v.get("spawned").and_then(|s| s.as_array()) {
        for it in items {
            let (Some(pane), Some(spawn_id)) = (
                it.get("pane").and_then(|x| x.as_str()).and_then(|s| Uuid::parse_str(s).ok()),
                it.get("spawn_id").and_then(|x| x.as_str()).map(String::from),
            ) else {
                continue;
            };
            let caller = it
                .get("caller_pane")
                .and_then(|x| x.as_str())
                .and_then(|s| Uuid::parse_str(s).ok())
                .unwrap_or(Uuid::nil());
            let callback = it.get("callback").and_then(|x| x.as_bool()).unwrap_or(false);
            let rec = SpawnRecord {
                spawn_id: spawn_id.clone(),
                caller_pane: caller,
                created_at: Instant::now(),
                callback,
                callback_fired: false,
            };
            reg.by_spawn.insert(spawn_id, pane);
            reg.by_pane.insert(pane, rec);
        }
    }
    reg
}

/// The user's answer to an ask-question.
#[derive(Debug, Clone, PartialEq)]
pub struct AskAnswer {
    /// selected choice indices (empty when only free text)
    pub choices: Vec<usize>,
    /// free-text answer (may be empty when a choice was picked)
    pub text: String,
}

/// One pending ask-user question. The agent's tool call stays blocked
/// (in the harness) until a client answers — there is NO timeout: the
/// user may take arbitrarily long. If the daemon restarts, the ask is
/// lost and the blocked poll sees "unknown" (the agent's escape hatch).
#[derive(Debug, Clone)]
pub struct AskRecord {
    pub ask_id: String,
    /// the pane that asked (nil = human-initiated)
    pub caller_pane: Uuid,
    pub session: Uuid,
    /// None until answered
    pub answer: Option<AskAnswer>,
    pub created_at: Instant,
    /// when the answer landed (drives the post-answer prune window)
    pub answered_at: Option<Instant>,
}

/// How long an answered ask stays in the registry after resolution
/// (the agent's status poll needs it for a bit).
pub const ASK_DONE_GRACE: std::time::Duration = std::time::Duration::from_secs(5 * 60);

impl AskRecord {
    pub fn state(&self) -> &'static str {
        match &self.answer {
            Some(_) => "answered",
            None => "pending",
        }
    }
}

/// The daemon's pending-question registry.
#[derive(Default)]
pub struct AskRegistry {
    pub pending: BTreeMap<String, AskRecord>,
}

impl AskRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn insert(&mut self, rec: AskRecord) {
        self.pending.insert(rec.ask_id.clone(), rec);
    }
    pub fn get(&self, ask_id: &str) -> Option<&AskRecord> {
        self.pending.get(ask_id)
    }
    pub fn get_mut(&mut self, ask_id: &str) -> Option<&mut AskRecord> {
        self.pending.get_mut(ask_id)
    }
    /// Drop resolved asks older than the grace window (the agent's
    /// status poll needs them briefly; pending asks are never pruned —
    /// asks have no expiry, the user may answer hours later).
    pub fn prune(&mut self) -> Vec<String> {
        let stale: Vec<String> = self
            .pending
            .values()
            .filter(|r| match r.answered_at {
                Some(at) => at.elapsed() > ASK_DONE_GRACE,
                None => false,
            })
            .map(|r| r.ask_id.clone())
            .collect();
        for id in &stale {
            self.pending.remove(id.as_str());
        }
        stale
    }
}

#[cfg(test)]
mod ask_tests {
    use super::*;

    fn rec(answered: bool) -> AskRecord {
        AskRecord {
            ask_id: "a1".into(),
            caller_pane: Uuid::new_v4(),
            session: Uuid::new_v4(),
            answer: answered.then_some(AskAnswer {
                choices: vec![0],
                text: String::new(),
            }),
            created_at: Instant::now(),
            answered_at: answered.then_some(Instant::now()),
        }
    }

    #[test]
    fn ask_states() {
        let r = rec(false);
        assert_eq!(r.state(), "pending");
        let r2 = rec(true);
        assert_eq!(r2.state(), "answered");
        let mut reg = AskRegistry::new();
        reg.insert(rec(false));
        assert!(reg.prune().is_empty()); // pending is kept
        let mut answered = rec(true);
        // fresh answer: within the grace window, kept
        reg.insert(answered.clone());
        assert!(reg.prune().is_empty());
        // stale answer: pruned
        answered.answered_at = Some(Instant::now() - ASK_DONE_GRACE - std::time::Duration::from_secs(1));
        *reg.get_mut("a1").unwrap() = answered;
        assert_eq!(reg.prune().len(), 1);
    }
}

/// The frame a completed/closed/denied spawn emits to clients.
pub fn agent_done_frame(spawn: &SpawnRecord, session: Uuid, pane: Uuid, outcome: &str, last: Option<ChatMsg>) -> Frame {
    let _ = spawn; // spawn_id is carried in the row for correlation
    Frame::AgentDone {
        spawn_id: spawn.spawn_id.clone(),
        session: session.to_string(),
        pane: pane.to_string(),
        outcome: outcome.into(),
        last_row: last,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(caller: Uuid, callback: bool) -> SpawnRecord {
        SpawnRecord {
            spawn_id: Uuid::new_v4().to_string(),
            caller_pane: caller,
            created_at: Instant::now(),
            callback,
            callback_fired: false,
        }
    }

    #[test]
    fn ownership_scoping() {
        let mut reg = SpawnRegistry::new();
        let caller = Uuid::new_v4();
        let child = Uuid::new_v4();
        let stranger = Uuid::new_v4();
        reg.register(child, rec(caller, false)).unwrap();

        assert!(reg.authorized(caller, child)); // parent -> child
        assert!(reg.authorized(child, child)); // self
        assert!(!reg.authorized(stranger, child)); // unrelated agent
        assert!(reg.authorized(Uuid::nil(), child)); // human
        assert!(!reg.authorized(child, stranger)); // child -> unrelated
    }

    #[test]
    fn cap_and_removal() {
        let mut reg = SpawnRegistry::new();
        let caller = Uuid::nil();
        for i in 0..MAX_SPAWNED_PANES {
            let p = Uuid::from_u128(i as u128 + 1);
            reg.register(p, rec(caller, false)).unwrap_or_else(|_| panic!("cap at {i}"));
        }
        let extra = Uuid::new_v4();
        assert!(reg.register(extra, rec(caller, false)).is_err());

        let some = Uuid::from_u128(7);
        assert!(reg.remove_pane(some).is_some());
        assert!(reg.register(extra, rec(caller, false)).is_ok()); // slot freed
        assert!(reg.get_by_pane(some).is_none());
    }

    #[test]
    fn pending_expiry() {
        let mut reg = SpawnRegistry::new();
        let caller = Uuid::new_v4();
        let child = Uuid::new_v4();
        let r = rec(caller, true);
        let sid = r.spawn_id.clone();
        reg.pending.insert(sid.clone(), (r.clone(), Instant::now()));
        // not expired yet
        assert!(reg.expire_pending().is_empty());
        // force expiry
        reg.pending.insert(sid.clone(), (r, Instant::now() - SPAWN_REQUEST_TTL));
        let expired = reg.expire_pending();
        assert_eq!(expired, vec![(sid, caller)]);
        assert!(reg.pending.is_empty());
        let _ = child;
    }

    #[test]
    fn registry_serializes() {
        let mut reg = SpawnRegistry::new();
        let caller = Uuid::new_v4();
        let child = Uuid::new_v4();
        let mut r = rec(caller, true);
        r.spawn_id = "sp-fixed".into();
        reg.register(child, r).unwrap();
        let v = registry_value(&reg);
        let back = registry_from_value(&v);
        assert!(back.authorized(caller, child));
        assert!(back.get_by_pane(child).unwrap().callback);
    }

    #[test]
    fn policy_parse() {
        assert_eq!(SpawnPolicy::parse("ask"), SpawnPolicy::Ask);
        assert_eq!(SpawnPolicy::parse("deny"), SpawnPolicy::Deny);
        assert_eq!(SpawnPolicy::parse("allow"), SpawnPolicy::Allow);
        assert_eq!(SpawnPolicy::parse("garbage"), SpawnPolicy::Allow);
    }
}
