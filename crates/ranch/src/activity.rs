//! H5.3 Activity View: daemon-side aggregation + the herd pause switch.
//!
//! **Worker shape** (AGENTS.md worker rules): a long-lived thread with
//! its OWN pipe pair. The poller writes `ActivityPush` frames into the
//! pipe's write end; the daemon's poll loop reads that pipe through a
//! synthetic "activity" client and broadcasts the frames to every
//! client like any other client frame. The thread never touches daemon
//! state — local data arrives through the shared [`LocalState`] that
//! the poll loop refreshes every tick, and stop arrives through a flag
//! (the daemon joins the thread on shutdown).
//!
//! **Polling** (every [`POLL_INTERVAL`]):
//! - local: pending asks, agent-spawned panes, recent trigger fires,
//!   recent machine-launched mule jobs (all from [`LocalState`], a
//!   10-minute window the poll loop prunes);
//! - forge: `GET /agents`, then per agent `GET /agents/:id/tasks`
//!   (non-terminal states), `GET /agents/:id/memory/beliefs?status=pending`
//!   and `GET /agents/:id/research?open=1` — each with a 10 s
//!   timeout. A failed feed (404/5xx/timeout) is skipped silently, the
//!   rest of the cycle still ships; a fully down forge degrades to
//!   local items + a one-time warning.
//! - mule: `GET /api/v1/wakes/activity?since=&limit=100` (RFC3339
//!   `since`; first poll = the last hour). No mule config ⇒ no feed,
//!   one-time warning, never a failure.
//!
//! **Diff + debounce**: the item set is compared against the last
//! broadcast set (item key = (kind, agent, id), full item equality).
//! On change the poller waits [`DEBOUNCE`], re-checks — coalescing
//! rapid churn (an ask that arrives and gets answered inside the
//! window never broadcasts) — and only then pushes the FULL new set.
//!
//! **Pause** (`AgentPause` / `ranch pause|resume`): per target agent,
//! forge `POST /agents/:id/pause|resume` + mule `POST
//! /api/v1/wakes/pause` (best-effort — no mule config means that leg
//! is skipped, still OK; per-leg failures land in
//! [`ranch_protocol::AgentPauseResult::error`]). No target = every
//! agent the daemon knows. Pending asks stay VISIBLE while an agent is
//! paused: pause halts new work, not observability (forge's reads stay
//! open for a paused agent by design).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ranch_protocol::{ActivityItem, Frame};

use super::agenttools::iso_from_unix;

/// Poll cadence (PLAN H5.3: every 5 s).
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Set-change debounce: coalesces churn within the window.
pub const DEBOUNCE: Duration = Duration::from_secs(2);
/// Per-feed HTTP timeout — a down forge must never stall the loop.
pub const FEED_TIMEOUT: Duration = Duration::from_secs(10);
/// Local/wake items leave the set after this (unix seconds).
pub const RECENT_WINDOW: i64 = 10 * 60;
/// Item summary cap (protocol contract: `ActivityItem.text` ≤ 200).
pub const SUMMARY_MAX: usize = 200;
/// Wake-log page size (mule caps at 100).
const WAKE_LIMIT: u32 = 100;

// ---------- shared local state ----------

/// The local half of the Activity View, handed to the poller by the
/// poll loop (refreshed in place every tick — the poller owns only
/// `agents`: its own `GET /agents` + the `AgentListOk` mirror).
/// `AgentPause` with no target resolves against `agents`.
#[derive(Default)]
pub struct LocalState {
    pub asks: Vec<ActivityItem>,
    pub spawns: Vec<ActivityItem>,
    pub fires: Vec<ActivityItem>,
    pub mule_jobs: Vec<ActivityItem>,
    /// agent id → name (the poller maintains it; see module docs)
    pub agents: BTreeMap<String, String>,
}

impl LocalState {
    /// The daemon-local items (no ordering — the poller sorts).
    pub fn local_items(&self) -> Vec<ActivityItem> {
        let mut v = Vec::new();
        v.extend_from_slice(&self.asks);
        v.extend_from_slice(&self.spawns);
        v.extend_from_slice(&self.fires);
        v.extend_from_slice(&self.mule_jobs);
        v
    }
}

// ---------- pure diff/debounce logic (unit-tested) ----------

/// One item's identity key: (kind, agent, id).
pub fn item_key(i: &ActivityItem) -> (String, Option<String>, String) {
    (i.kind.clone(), i.agent.clone(), i.id.clone())
}

/// True when the sets differ in membership OR any item's content
/// (two sets of the same items, in any order, compare equal).
pub fn set_changed(last: &[ActivityItem], new: &[ActivityItem]) -> bool {
    let a: BTreeMap<_, _> = last.iter().map(|i| (item_key(i), i)).collect();
    let b: BTreeMap<_, _> = new.iter().map(|i| (item_key(i), i)).collect();
    a != b
}

/// One-line summary: whitespace-trimmed, truncated to `SUMMARY_MAX`
/// chars (char-boundary safe) with an ellipsis.
pub fn truncate_summary(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= SUMMARY_MAX {
        return s.to_string();
    }
    let cut: String = s.chars().take(SUMMARY_MAX - 1).collect();
    format!("{cut}…")
}

/// Most-recent-first. RFC3339-UTC strings sort lexicographically; the
/// sort is stable, so ties keep their source order.
pub fn sort_newest(items: &mut [ActivityItem]) {
    items.sort_by(|a, b| b.ts.cmp(&a.ts));
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn iso_now() -> String {
    iso_from_unix(now_unix())
}

// ---------- forge feeds (H5.3 part 1 endpoints) ----------

/// GET helper for the poller: X-API-Key auth, 10 s timeout, 4xx/5xx
/// come back as Err (status-as-error off, like forge.rs `http_json`).
fn forge_get(cfg: &crate::forge::ForgeConfig, path: &str) -> Result<serde_json::Value, String> {
    use std::io::Read as _;
    let url = format!("{}{}", cfg.base, path);
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(FEED_TIMEOUT))
            .build(),
    );
    let mut res = agent
        .get(&url)
        .header("X-API-Key", &cfg.key)
        .call()
        .map_err(|e| format!("forge GET {path}: {e}"))?;
    let mut text = String::new();
    res.body_mut()
        .as_reader()
        .read_to_string(&mut text)
        .map_err(|e| format!("forge read {path}: {e}"))?;
    if !res.status().is_success() {
        return Err(format!("forge GET {path}: HTTP {}", res.status()));
    }
    serde_json::from_str(&text).map_err(|e| format!("forge decode {path}: {e}"))
}

/// `GET /agents` → id → name.
pub fn parse_agents(v: &serde_json::Value) -> BTreeMap<String, String> {
    let arr = v
        .get("agents")
        .and_then(|x| x.as_array())
        .cloned()
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();
    let mut out = BTreeMap::new();
    for a in arr {
        if let (Some(id), Some(name)) = (
            a.get("id").and_then(|x| x.as_str()),
            a.get("name").and_then(|x| x.as_str()),
        ) {
            out.insert(id.to_string(), name.to_string());
        }
    }
    out
}

/// `GET /agents/:id/tasks` → "forge_task" items. Durable tasks carry
/// no row timestamp (recency ≈ task id, documented forge-side), so
/// `ts` = the fetch time.
pub fn parse_forge_tasks(v: &serde_json::Value, agent: &str, ts: &str) -> Vec<ActivityItem> {
    let arr = v
        .get("tasks")
        .and_then(|x| x.as_array())
        .cloned()
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();
    arr.iter()
        .filter_map(|t| {
            let id = t
                .get("task_id")
                .and_then(|x| x.as_i64())
                .map(|n| n.to_string())
                .or_else(|| t.get("task_id").and_then(|x| x.as_str()).map(String::from))?;
            let state = t.get("state").and_then(|x| x.as_str()).unwrap_or("");
            let kind = t.get("kind").and_then(|x| x.as_str()).unwrap_or("");
            Some(ActivityItem {
                id: format!("forge_task/{agent}/{id}"),
                kind: "forge_task".into(),
                agent: Some(agent.into()),
                text: truncate_summary(&format!("task #{id} ({kind}) — {state}")),
                ts: ts.into(),
            })
        })
        .collect()
}

/// `GET /agents/:id/memory/beliefs?status=pending` →
/// "belief_pending" items (ts = the belief's `updated_at`).
pub fn parse_forge_beliefs(v: &serde_json::Value, agent: &str) -> Vec<ActivityItem> {
    let arr = v
        .get("beliefs")
        .and_then(|x| x.as_array())
        .cloned()
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();
    arr.iter()
        .filter_map(|b| {
            let id = b.get("id").and_then(|x| x.as_str())?.to_string();
            let content = b.get("content").and_then(|x| x.as_str()).unwrap_or("");
            let ts = b
                .get("updated_at")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            Some(ActivityItem {
                id: format!("belief_pending/{agent}/{id}"),
                kind: "belief_pending".into(),
                agent: Some(agent.into()),
                text: truncate_summary(&format!("belief: {content}")),
                ts: if ts.is_empty() { iso_now() } else { ts },
            })
        })
        .collect()
}

/// `GET /agents/:id/research?open=1` → "research_open" items
/// (ts = the task's `created_at`).
pub fn parse_forge_research(v: &serde_json::Value, agent: &str) -> Vec<ActivityItem> {
    let arr = v
        .get("research")
        .and_then(|x| x.as_array())
        .cloned()
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();
    arr.iter()
        .filter_map(|r| {
            let id = r.get("id").and_then(|x| x.as_str())?.to_string();
            let question = r.get("question").and_then(|x| x.as_str()).unwrap_or("");
            let state = r.get("state").and_then(|x| x.as_str()).unwrap_or("");
            let ts = r
                .get("created_at")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            Some(ActivityItem {
                id: format!("research_open/{agent}/{id}"),
                kind: "research_open".into(),
                agent: Some(agent.into()),
                text: truncate_summary(&format!("research: {question} ({state})")),
                ts: if ts.is_empty() { iso_now() } else { ts },
            })
        })
        .collect()
}

// ---------- mule feeds ----------

/// Mule HTTP with a hard cap, for the pause path: `handle_agent_pause`
/// runs on the daemon's poll loop (sync-localhost pattern), so a hung
/// mule must never wedge the loop. (The poller thread shares the cap
/// too — it keeps daemon shutdown joins bounded. The mule worker's
/// own long-lived calls keep using `mule::http_json` un-capped.)
fn mule_http_timed(
    cfg: &super::mule::MuleConfig,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    use std::io::Read as _;
    let url = format!("{}{}", cfg.base, path);
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(FEED_TIMEOUT))
            .build(),
    );
    let key = cfg.api_key.clone();
    let sent = match method.to_ascii_uppercase().as_str() {
        "GET" => {
            let mut r = agent.get(&url);
            if let Some(k) = &key {
                r = r.header("Authorization", &format!("Bearer {k}"));
            }
            r.call()
        }
        "POST" => {
            let mut r = agent.post(&url);
            if let Some(k) = &key {
                r = r.header("Authorization", &format!("Bearer {k}"));
            }
            r.send_json(body.cloned().unwrap_or(serde_json::Value::Null))
        }
        _ => return Err(format!("mule: unsupported method {method}")),
    };
    let mut res = sent.map_err(|e| format!("mule {method} {path}: {e}"))?;
    let mut text = String::new();
    res.body_mut()
        .as_reader()
        .read_to_string(&mut text)
        .map_err(|e| format!("mule read {path}: {e}"))?;
    if !res.status().is_success() {
        return Err(format!("mule {method} {path}: HTTP {}", res.status()));
    }
    serde_json::from_str(&text).map_err(|e| format!("mule decode {path}: {e}"))
}

/// `POST /api/v1/wakes/pause` body (exact JSON, unit-tested).
pub fn mule_pause_body(agent_id: &str, paused: bool) -> serde_json::Value {
    serde_json::json!({ "agent_id": agent_id, "paused": paused })
}

/// The mule pause response's flip count (`"updated"`).
pub fn parse_mule_pause(v: &serde_json::Value) -> Option<usize> {
    v.get("updated")
        .and_then(|x| x.as_u64())
        .map(|n| n as usize)
}

/// Flip the agent's wakes (best-effort leg of the pause switch).
pub fn mule_wakes_pause(
    cfg: &super::mule::MuleConfig,
    agent_id: &str,
    paused: bool,
) -> Result<usize, String> {
    let body = mule_pause_body(agent_id, paused);
    let v = mule_http_timed(cfg, "POST", "/api/v1/wakes/pause", Some(&body))?;
    parse_mule_pause(&v)
        .ok_or_else(|| "mule POST /api/v1/wakes/pause: unexpected response shape".into())
}

/// `GET /api/v1/wakes/activity` query string (RFC3339 since — mule's
/// contract; unit-tested).
pub fn mule_activity_query(since_unix: i64, limit: u32) -> String {
    format!("?since={}&limit={limit}", iso_from_unix(since_unix))
}

/// `GET /api/v1/wakes/activity` → "wake" items. The feed is the
/// caller's recent wake_log rows; items older than the 10-minute
/// window are dropped so the timeline stays bounded (the `since`
/// watermark only moves forward).
pub fn parse_mule_activity(v: &serde_json::Value, now: i64) -> Vec<ActivityItem> {
    let arr = v
        .get("activity")
        .and_then(|x| x.as_array())
        .cloned()
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();
    arr.iter()
        .filter_map(|w| {
            let wake_id = w.get("wake_id").and_then(|x| x.as_str())?.to_string();
            let name = w
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let kind = w
                .get("kind")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let fired_at = w
                .get("fired_at")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let ok = w.get("ok").and_then(|x| x.as_bool()).unwrap_or(true);
            let error = w.get("error").and_then(|x| x.as_str()).unwrap_or("");
            let mut text = format!("wake {name} ({kind})");
            if !ok {
                if error.is_empty() {
                    text.push_str(" — failed");
                } else {
                    text.push_str(" — ");
                    text.push_str(error);
                }
            }
            Some(ActivityItem {
                id: format!("wake/{wake_id}"),
                kind: "wake".into(),
                agent: None, // the wake_log row carries no agent
                text: truncate_summary(&text),
                ts: if fired_at.is_empty() {
                    iso_now()
                } else {
                    fired_at
                },
            })
        })
        .filter(|i| {
            // keep only items within the window (ts strings are
            // RFC3339-UTC; compare against the window edge the same way)
            i.ts >= iso_from_unix(now - RECENT_WINDOW)
        })
        .collect()
}

/// The mule wake feed since the last poll (first poll: the last hour).
fn mule_wakes_activity(
    cfg: &super::mule::MuleConfig,
    since_unix: i64,
) -> Result<Vec<ActivityItem>, String> {
    let v = mule_http_timed(
        cfg,
        "GET",
        &mule_activity_query(since_unix, WAKE_LIMIT),
        None,
    )?;
    Ok(parse_mule_activity(&v, now_unix()))
}

// ---------- the poller ----------

/// What the poller may reach for remotely.
#[derive(Clone)]
pub struct PollerConfig {
    pub forge: Option<crate::forge::ForgeConfig>,
    pub mule: Option<super::mule::MuleConfig>,
}

struct PollerState {
    /// wake-activity watermark (unix); advances on success only, so a
    /// failed poll re-fetches the window it missed
    mule_since: i64,
    forge_warned: bool,
    mule_warned: bool,
}

/// Start the poller thread. `pipe_w` is the write end of the poller's
/// private pipe (the daemon polls the read end as the "activity"
/// client); `shared` is the local-state snapshot the poll loop
/// refreshes; `stop` + the daemon's join give a clean shutdown.
pub fn spawn_poller(
    cfg: PollerConfig,
    pipe_w: Arc<Mutex<std::fs::File>>,
    shared: Arc<Mutex<LocalState>>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("activity-poller".into())
        .spawn(move || {
            let mut last_broadcast: Vec<ActivityItem> = Vec::new();
            let mut st = PollerState {
                mule_since: now_unix().saturating_sub(3600), // first poll: last 1 h
                forge_warned: false,
                mule_warned: false,
            };
            loop {
                if !sleep_interruptible(POLL_INTERVAL, &stop) {
                    break;
                }
                let set = compute_set(&cfg, &shared, &mut st);
                if !set_changed(&last_broadcast, &set) {
                    continue;
                }
                // debounce + re-check: churn inside the window that
                // settles back to the last broadcast set never ships
                if !sleep_interruptible(DEBOUNCE, &stop) {
                    break;
                }
                let set = compute_set(&cfg, &shared, &mut st);
                if !set_changed(&last_broadcast, &set) {
                    continue;
                }
                if write_push(&pipe_w, &set) {
                    last_broadcast = set;
                }
            }
            eprintln!("ranchd: activity: poller stopped");
        })
        .expect("spawn activity poller")
}

/// Sleep in 250 ms slices so stop is honored promptly. Returns false
/// when stop was requested mid-sleep.
fn sleep_interruptible(d: Duration, stop: &AtomicBool) -> bool {
    let step = Duration::from_millis(250);
    let mut left = d;
    while left > Duration::ZERO {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let chunk = if left > step { step } else { left };
        std::thread::sleep(chunk);
        left -= chunk;
    }
    true
}

/// One poll cycle: local items + forge feeds + mule wakes, sorted
/// newest-first.
fn compute_set(
    cfg: &PollerConfig,
    shared: &Arc<Mutex<LocalState>>,
    st: &mut PollerState,
) -> Vec<ActivityItem> {
    let mut items = Vec::new();
    {
        let guard = shared.lock().unwrap_or_else(|e| e.into_inner());
        items.extend(guard.local_items());
    }
    if let Some(fc) = &cfg.forge {
        match forge_get(fc, "/agents") {
            Ok(v) => {
                let agents = parse_agents(&v);
                for (id, name) in &agents {
                    // per-feed failures are skipped silently: a broken
                    // feed must never stall the loop or drop the rest
                    let ts = iso_now();
                    if let Ok(t) =
                        forge_get(fc, &format!("/agents/{id}/tasks?state=pending,running"))
                    {
                        items.extend(parse_forge_tasks(&t, name, &ts));
                    }
                    if let Ok(b) =
                        forge_get(fc, &format!("/agents/{id}/memory/beliefs?status=pending"))
                    {
                        items.extend(parse_forge_beliefs(&b, name));
                    }
                    if let Ok(r) = forge_get(fc, &format!("/agents/{id}/research?open=1")) {
                        items.extend(parse_forge_research(&r, name));
                    }
                }
                if !agents.is_empty() {
                    let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
                    for (id, name) in &agents {
                        guard.agents.insert(id.clone(), name.clone());
                    }
                }
            }
            Err(e) if !st.forge_warned => {
                st.forge_warned = true;
                eprintln!(
                    "ranchd: activity: forge agent list unavailable ({e}); \
                     forge feeds degraded until it recovers"
                );
            }
            Err(_) => {}
        }
    }
    if let Some(mc) = &cfg.mule {
        match mule_wakes_activity(mc, st.mule_since) {
            Ok(wakes) => {
                items.extend(wakes);
                st.mule_since = now_unix(); // watermark advances on success
            }
            Err(e) if !st.mule_warned => {
                st.mule_warned = true;
                eprintln!(
                    "ranchd: activity: mule wake activity unavailable ({e}); \
                     wake feed degraded until it recovers"
                );
            }
            Err(_) => {}
        }
    }
    sort_newest(&mut items);
    items
}

/// Write one `ActivityPush` into the pipe. Returns false (no
/// broadcast bookkeeping) when the write fails — the next cycle
/// retries because `last_broadcast` is untouched.
fn write_push(pipe_w: &Arc<Mutex<std::fs::File>>, items: &[ActivityItem]) -> bool {
    use std::io::Write as _;
    let cid = uuid::Uuid::new_v4().to_string();
    let frame = Frame::ActivityPush {
        items: items.to_vec(),
    };
    let mut out = String::new();
    for line in ranch_protocol::encode_frame(&frame, &cid) {
        out.push_str(&line);
        out.push('\n');
    }
    let Ok(mut f) = pipe_w.lock() else {
        eprintln!("ranchd: activity: pipe lock poisoned; push skipped");
        return false;
    };
    match f.write_all(out.as_bytes()).and_then(|_| f.flush()) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("ranchd: activity: pipe write failed: {e}; push skipped");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(id: &str, kind: &str, agent: Option<&str>, text: &str, ts: &str) -> ActivityItem {
        ActivityItem {
            id: id.into(),
            kind: kind.into(),
            agent: agent.map(String::from),
            text: text.into(),
            ts: ts.into(),
        }
    }

    #[test]
    fn set_changed_keys_on_kind_agent_id() {
        let a = vec![
            item("1", "ask", None, "q?", "2026-01-01T00:00:00Z"),
            item("2", "wake", None, "w", "2026-01-01T00:01:00Z"),
        ];
        // same items, different order ⇒ unchanged
        let b = vec![
            item("2", "wake", None, "w", "2026-01-01T00:01:00Z"),
            item("1", "ask", None, "q?", "2026-01-01T00:00:00Z"),
        ];
        assert!(!set_changed(&a, &b));
        // a changed summary of the same key ⇒ changed
        let c = vec![
            item("1", "ask", None, "q changed", "2026-01-01T00:00:00Z"),
            item("2", "wake", None, "w", "2026-01-01T00:01:00Z"),
        ];
        assert!(set_changed(&a, &c));
        // same key, different kind ⇒ different items
        let d = vec![
            item("1", "spawn", None, "q?", "2026-01-01T00:00:00Z"),
            item("2", "wake", None, "w", "2026-01-01T00:01:00Z"),
        ];
        assert!(set_changed(&a, &d));
        // empty vs empty
        assert!(!set_changed(&[], &[]));
        assert!(set_changed(&a, &[]));
    }

    #[test]
    fn truncate_summary_caps_at_200_chars_char_safe() {
        assert_eq!(truncate_summary("short"), "short");
        let long = "é".repeat(300); // non-ASCII: byte truncation would panic-ish/mangle
        let out = truncate_summary(&long);
        assert_eq!(out.chars().count(), SUMMARY_MAX);
        assert!(out.ends_with('…'));
        let ascii = "x".repeat(SUMMARY_MAX + 10);
        let out = truncate_summary(&ascii);
        assert_eq!(out.chars().count(), SUMMARY_MAX);
        assert!(out.starts_with(&"x".repeat(SUMMARY_MAX - 1)));
    }

    #[test]
    fn sort_newest_orders_by_ts_desc_stable() {
        let mut v = vec![
            item("a", "ask", None, "x", "2026-01-01T00:00:00Z"),
            item("b", "ask", None, "x", "2026-01-01T00:02:00Z"),
            item("c", "ask", None, "x", "2026-01-01T00:01:00Z"),
        ];
        sort_newest(&mut v);
        assert_eq!(
            v.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["b", "c", "a"]
        );
        // equal ts keeps source order
        let mut v = vec![
            item("a", "ask", None, "x", "2026-01-01T00:00:00Z"),
            item("b", "ask", None, "x", "2026-01-01T00:00:00Z"),
        ];
        sort_newest(&mut v);
        assert_eq!(
            v.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn parse_agents_handles_object_and_bare_array() {
        let v = json!({"agents": [{"id": "a1", "name": "iris"}, {"id": "a2"}]});
        let m = parse_agents(&v);
        assert_eq!(m.get("a1").map(String::as_str), Some("iris"));
        assert!(!m.contains_key("a2"));
        let v = json!([{"id": "a1", "name": "iris"}]);
        assert_eq!(parse_agents(&v).get("a1").map(String::as_str), Some("iris"));
    }

    #[test]
    fn parse_forge_tasks_items() {
        let v = json!({"tasks": [
            {"task_id": 7, "state": "running", "kind": "tool", "parent_task_id": null},
            {"task_id": 8, "state": "pending", "kind": "research", "parent_task_id": "5"},
        ]});
        let items = parse_forge_tasks(&v, "iris", "2026-01-01T00:00:00Z");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "forge_task/iris/7");
        assert_eq!(items[0].kind, "forge_task");
        assert_eq!(items[0].agent.as_deref(), Some("iris"));
        assert_eq!(items[0].text, "task #7 (tool) — running");
        assert_eq!(items[0].ts, "2026-01-01T00:00:00Z");
    }

    #[test]
    fn parse_forge_beliefs_and_research_items() {
        let v = json!({"beliefs": [
            {"id": "b1", "content": "ranch is the presence plane", "updated_at": "2026-01-02T00:00:00Z"}
        ]});
        let items = parse_forge_beliefs(&v, "iris");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "belief_pending/iris/b1");
        assert_eq!(items[0].kind, "belief_pending");
        assert_eq!(items[0].text, "belief: ranch is the presence plane");
        assert_eq!(items[0].ts, "2026-01-02T00:00:00Z");

        let v = json!({"research": [
            {"id": "r1", "question": "how does X work?", "state": "running", "created_at": "2026-01-03T00:00:00Z"}
        ]});
        let items = parse_forge_research(&v, "iris");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "research_open/iris/r1");
        assert_eq!(items[0].kind, "research_open");
        assert_eq!(items[0].text, "research: how does X work? (running)");
    }

    #[test]
    fn mule_pause_body_is_exact() {
        let b = mule_pause_body("a1", true);
        assert_eq!(b, json!({"agent_id": "a1", "paused": true}));
        let b = mule_pause_body("a1", false);
        assert_eq!(b, json!({"agent_id": "a1", "paused": false}));
    }

    #[test]
    fn parse_mule_pause_reads_updated() {
        let v = json!({"agent_id": "a1", "paused": true, "updated": 3});
        assert_eq!(parse_mule_pause(&v), Some(3));
        assert_eq!(parse_mule_pause(&json!({})), None);
    }

    #[test]
    fn mule_activity_query_is_rfc3339() {
        let q = mule_activity_query(1789208400, 100);
        // 1789208400 = 2026-09-12T10:20:00Z
        assert_eq!(q, "?since=2026-09-12T10:20:00Z&limit=100");
    }

    #[test]
    fn parse_mule_activity_items_and_window() {
        // now = 1970-01-02T03:46:40Z; window edge = now - 10 min
        let now = 100_000;
        let fresh = "1970-01-02T03:46:00Z";
        let stale = "1970-01-02T03:30:40Z"; // >10 min old: outside the window
        let v = json!({"activity": [
            {"wake_id": "w1", "name": "nightly", "kind": "cron", "fired_at": fresh, "job_id": "j1", "ok": true},
            {"wake_id": "w2", "name": "flaky", "kind": "event", "fired_at": fresh, "job_id": "j2", "ok": false, "error": "boom"},
            {"wake_id": "w3", "name": "old", "kind": "cron", "fired_at": stale, "job_id": "j3", "ok": true},
        ]});
        let items = parse_mule_activity(&v, now);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "wake/w1");
        assert_eq!(items[0].kind, "wake");
        assert_eq!(items[0].agent, None);
        assert_eq!(items[0].text, "wake nightly (cron)");
        assert!(items[1].text.ends_with("— boom"));
    }

    #[test]
    fn local_state_local_items_covers_all_halves() {
        let st = LocalState {
            asks: vec![item("ask/1", "ask", None, "a", "t1")],
            spawns: vec![item("spawn/1", "spawn", None, "s", "t2")],
            fires: vec![item("trigger/x", "trigger", None, "f", "t3")],
            mule_jobs: vec![item("mule_job/1", "mule_job", None, "m", "t4")],
            agents: BTreeMap::new(),
        };
        assert_eq!(st.local_items().len(), 4);
    }
}
