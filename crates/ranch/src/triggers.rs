//! Triggers (Phase D): cron + event-based workflow execution.
//!
//! The daemon owns the schedule; Supabase mirrors rows for dashboards.
//! Three trigger types (SPEC §6.2):
//! - **cron** — expression + timezone, evaluated in the daemon tick
//! - **event** — predicates over ranch events (agent turn ended,
//!   workflow completed, file changed, pane exited, webhook)
//! - **webhook** — matches WebhookEvent frames (Phase E; same registry)
//!
//! Fire = mule `POST /api/v1/jobs` via the mule worker + `last_run`
//! bookkeeping + a `TriggerFired` broadcast.
//!
//! Cron parsing: the repo avoids heavy deps on the static build path,
//! so this is a minimal 5-field cron (m h dom mon dow) with `*`, `*/n`,
//! lists `a,b`, and ranges `a-b`. Timezones: schedules run in **UTC**
//! (v1; `tz` is parsed but only UTC is honored — documented in SPEC).

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use ranch_protocol::Frame;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq)]
pub enum TriggerKind {
    Cron { expr: String },
    Event { event: String, filter: serde_json::Value },
    Webhook { source: String, event_tag: Option<String> },
}

#[derive(Debug, Clone)]
pub struct Trigger {
    pub id: Uuid,
    pub name: String,
    pub workflow_id: String,
    pub kind: TriggerKind,
    /// job input (templated at fire time from the event payload)
    pub input: Option<serde_json::Value>,
    pub enabled: bool,
    /// optional catch-up-on-reconnect for cron (fire once if the
    /// previous due time was missed while offline)
    pub catch_up: bool,
    /// last fire (job id + unix ts + status) — bookkeeping/dashboards
    pub last_run: Option<(String, i64, String)>,
}

impl Trigger {
    pub fn kind_tag(&self) -> &'static str {
        match &self.kind {
            TriggerKind::Cron { .. } => "cron",
            TriggerKind::Event { .. } => "event",
            TriggerKind::Webhook { .. } => "webhook",
        }
    }

    pub fn spec_value(&self) -> serde_json::Value {
        match &self.kind {
            TriggerKind::Cron { expr } => serde_json::json!({ "cron": expr, "tz": "UTC" }),
            TriggerKind::Event { event, filter } => {
                serde_json::json!({ "event": event, "filter": filter })
            }
            TriggerKind::Webhook { source, event_tag } => {
                serde_json::json!({ "source": source, "event": event_tag })
            }
        }
    }

    /// Does this trigger match a fired ranch event?
    /// `event_type`: "agent_turn_ended" | "workflow_completed" |
    /// "file_changed" | "pane_exited" | "webhook".
    pub fn matches_event(&self, event_type: &str, payload: &serde_json::Value) -> bool {
        let TriggerKind::Event { event, filter } = &self.kind else {
            return false;
        };
        if event != event_type {
            return false;
        }
        // filter: {key: value} — every pair must match the payload
        if let Some(f) = filter.as_object() {
            for (k, want) in f {
                let got = payload.get(k).cloned().unwrap_or(serde_json::Value::Null);
                if &got != want {
                    return false;
                }
            }
        }
        true
    }

    /// Does this trigger match an inbound webhook?
    pub fn matches_webhook(&self, source: &str, event_tag: &str) -> bool {
        match &self.kind {
            TriggerKind::Webhook { source: s, event_tag: want } => {
                if s != source {
                    return false;
                }
                match want {
                    Some(w) => w == event_tag,
                    None => true,
                }
            }
            _ => false,
        }
    }

    /// The job input, with `{event.payload.x}` substitutions applied.
    pub fn job_input(&self, event_payload: Option<&serde_json::Value>) -> Option<serde_json::Value> {
        let base = self.input.clone()?;
        let Some(event_payload) = event_payload else {
            return Some(base);
        };
        Some(substitute(&base, event_payload))
    }
}

/// Template substitution: walks the input; strings equal to
/// "{event.payload.path.to}" get replaced by that value (stringified);
/// other strings keep `{...}` literals.
fn substitute(input: &serde_json::Value, payload: &serde_json::Value) -> serde_json::Value {
    match input {
        serde_json::Value::String(s) => {
            let inner = s.trim().trim_start_matches('{').trim_end_matches('}');
            if s.starts_with('{') && s.ends_with('}') && inner.starts_with("event.") {
                let path = inner.trim_start_matches("event.");
                let v = payload
                    .pointer(&format!("/{}/", "").replace("//", "/")) // noop guard
                    .or_else(|| payload.get(path))
                    .or_else(|| {
                        // dotted path support: payload.a.b
                        let mut cur = payload;
                        for part in path.split('.') {
                            cur = cur.get(part)?;
                        }
                        Some(cur)
                    });
                match v {
                    Some(serde_json::Value::String(sv)) => serde_json::Value::String(sv.clone()),
                    Some(other) => other.clone(),
                    None => input.clone(), // unresolved: keep literal
                }
            } else {
                input.clone()
            }
        }
        serde_json::Value::Object(map) => {
            serde_json::Value::Object(map.iter().map(|(k, v)| (k.clone(), substitute(v, payload))).collect())
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(|v| substitute(v, payload)).collect())
        }
        other => other.clone(),
    }
}

// ---------- minimal cron ----------

/// Evaluate a 5-field cron expression against a UTC timestamp.
/// Fields: minute hour day-of-month month day-of-week. Supports `*`,
/// `*/n`, lists `a,b,c`, ranges `a-b`. Returns true when `ts` matches.
pub fn cron_matches(expr: &str, ts: i64) -> bool {
    let (_, mo, d, h, mi, dow) = dow_from_unix(ts);
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return false;
    }
    field_matches(fields[0], mi as u32, 0, 59)
        && field_matches(fields[1], h as u32, 0, 23)
        && field_matches(fields[2], d as u32, 1, 31)
        && field_matches(fields[3], mo as u32, 1, 12)
        && field_matches(fields[4], dow, 0, 6)
}

/// Next fire time (unix secs) strictly after `from` for a cron expr,
/// probing minute by minute (bounded: 366 days). Minimal but correct
/// for the field subset above.
pub fn cron_next(expr: &str, from: i64) -> Option<i64> {
    let from_min = from.div_euclid(60) * 60;
    for i in 1..=(366 * 24 * 60) {
        let t = from_min + i * 60;
        if cron_matches(expr, t) {
            return Some(t);
        }
    }
    None
}

/// Previous fire time (unix secs) at or before `from` — used by
/// catch_up-on-reconnect.
pub fn cron_prev(expr: &str, at: i64) -> Option<i64> {
    let at_min = at.div_euclid(60) * 60;
    for i in 0..=(366 * 24 * 60) {
        let t = at_min - i * 60;
        if cron_matches(expr, t) {
            return Some(t);
        }
    }
    None
}

fn field_matches(field: &str, value: u32, min: u32, max: u32) -> bool {
    for part in field.split(',') {
        // range or step form
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().unwrap_or(1)),
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            match (a.parse::<u32>(), b.parse::<u32>()) {
                (Ok(a), Ok(b)) => (a, b),
                _ => continue,
            }
        } else {
            // single value (possibly with step — degenerate)
            match range.parse::<u32>() {
                Ok(v) => (v, if step > 1 { max } else { v }),
                _ => continue,
            }
        };
        if value >= lo && value <= hi && (value - lo) % step == 0 {
            return true;
        }
    }
    false
}

/// (year, month, day, hour, minute, day-of-week 0=Sun) from unix secs.
pub fn dow_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let (y, mo, d, h, mi, _s) = crate::daemon::civil_from_unix(secs);
    // day-of-week: unix epoch (1970-01-01) was a Thursday (4)
    let days = secs.div_euclid(86_400);
    let dow = (days.rem_euclid(7) + 4).rem_euclid(7) as u32;
    (y, mo, d, h, mi, dow)
}

/// The trigger scheduler: owns the registry + fires workflows.
pub struct Scheduler {
    pub triggers: BTreeMap<Uuid, Trigger>,
    /// last cron fire per trigger (unix minute) — catch-up + dedupe
    last_cron: BTreeMap<Uuid, i64>,
    /// configured: mule worker tx (None = nothing can fire)
    mule_tx: Option<std::sync::mpsc::Sender<crate::daemon::mule::MuleJob>>,
}

impl Scheduler {
    pub fn new(mule_tx: Option<std::sync::mpsc::Sender<crate::daemon::mule::MuleJob>>) -> Self {
        Scheduler {
            triggers: BTreeMap::new(),
            last_cron: BTreeMap::new(),
            mule_tx,
        }
    }

    pub fn now() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// Catch-up on boot: fire crons whose due time passed while offline
    /// (only `catch_up: true` triggers; once each).
    pub fn catch_up(&mut self) {
        let now = Self::now();
        let ids: Vec<Uuid> = self.triggers.keys().copied().collect();
        for id in ids {
            let (Some(mule_tx), Some(trig)) = (self.mule_tx.as_ref(), self.triggers.get(&id)) else {
                continue;
            };
            let TriggerKind::Cron { expr } = &trig.kind else { continue };
            if !trig.enabled || !trig.catch_up {
                continue;
            }
            // if a scheduled fire was missed since the last one we saw
            let last_seen = self.last_cron.get(&id).copied().unwrap_or_else(|| {
                trig.last_run.as_ref().map(|(_, ts, _)| *ts).unwrap_or(now)
            });
            let Some(due) = cron_prev(expr, now) else { continue };
            if due > last_seen && now - due < 86_400 {
                // missed one within the last day: fire it
                let wf = trig.workflow_id.clone();
                let input = trig.input.clone();
                let _ = mule_tx.send(crate::daemon::mule::MuleJob::Run {
                    req_id: Uuid::new_v4().to_string(),
                    workflow_id: wf,
                    input,
                    session: Uuid::nil(), // worker creates no pane for catch-up runs
                    pane: Uuid::nil(),
                });
                self.last_cron.insert(id, due);
                if let Some(t) = self.triggers.get_mut(&id) {
                    t.last_run = Some((Uuid::new_v4().to_string(), due, "catchup".into()));
                }
            }
        }
    }

    /// Tick: fire due cron triggers (called ~1/min from the daemon loop).
    /// Returns the frames to broadcast (TriggerFired) + mirror updates.
    pub fn tick(&mut self) -> (Vec<Frame>, Vec<crate::relay::RelayOut>) {
        let now = Self::now();
        let this_min = now.div_euclid(60) * 60;
        let mut frames = Vec::new();
        let mut mirrors = Vec::new();
        let ids: Vec<Uuid> = self.triggers.keys().copied().collect();
        for id in ids {
            let (Some(_), Some(trig)) = (self.mule_tx.as_ref(), self.triggers.get(&id)) else {
                continue;
            };
            let TriggerKind::Cron { expr } = &trig.kind else { continue };
            if !trig.enabled || !cron_matches(expr, this_min) {
                continue;
            }
            // once per minute per trigger
            if self.last_cron.get(&id).copied() == Some(this_min) {
                continue;
            }
            self.last_cron.insert(id, this_min);
            let job_id = Uuid::new_v4().to_string();
            if let Some(tx) = &self.mule_tx {
                let _ = tx.send(crate::daemon::mule::MuleJob::Run {
                    req_id: Uuid::new_v4().to_string(),
                    workflow_id: trig.workflow_id.clone(),
                    input: trig.input.clone(),
                    session: Uuid::nil(),
                    pane: Uuid::nil(),
                });
            }
            if let Some(t) = self.triggers.get_mut(&id) {
                t.last_run = Some((job_id.clone(), this_min, "queued".into()));
            }
            frames.push(Frame::TriggerFired {
                trigger: id.to_string(),
                job: job_id.clone(),
            });
            if let Some(t) = self.triggers.get(&id) {
                mirrors.push(crate::relay::RelayOut::UpsertTrigger {
                    id: id.to_string(),
                    name: t.name.clone(),
                    workflow_id: t.workflow_id.clone(),
                    kind: t.kind_tag().into(),
                    enabled: t.enabled,
                    spec: t.spec_value(),
                });
            }
        }
        (frames, mirrors)
    }

    /// Fire an event-matched trigger (agent turn ended, webhook, …).
    pub fn fire_event(&mut self, event_type: &str, payload: &serde_json::Value) -> (Vec<Frame>, Vec<crate::relay::RelayOut>) {
        let mut frames = Vec::new();
        let mut mirrors = Vec::new();
        let ids: Vec<Uuid> = self.triggers.keys().copied().collect();
        for id in ids {
            let Some(trig) = self.triggers.get(&id) else { continue };
            let is_webhook = matches!(trig.kind, TriggerKind::Webhook { .. });
            let hit = if is_webhook {
                let src = payload.get("source").and_then(|x| x.as_str()).unwrap_or("");
                let tag = payload.get("event").and_then(|x| x.as_str()).unwrap_or("");
                trig.matches_webhook(src, tag)
            } else {
                trig.matches_event(event_type, payload)
            };
            if !hit || !trig.enabled {
                continue;
            }
            let job_id = Uuid::new_v4().to_string();
            if let Some(tx) = &self.mule_tx {
                let _ = tx.send(crate::daemon::mule::MuleJob::Run {
                    req_id: Uuid::new_v4().to_string(),
                    workflow_id: trig.workflow_id.clone(),
                    input: trig.job_input(Some(payload)),
                    session: Uuid::nil(),
                    pane: Uuid::nil(),
                });
            }
            if let Some(t) = self.triggers.get_mut(&id) {
                t.last_run = Some((job_id.clone(), Self::now(), "queued".into()));
            }
            frames.push(Frame::TriggerFired {
                trigger: id.to_string(),
                job: job_id.clone(),
            });
            if let Some(t) = self.triggers.get(&id) {
                mirrors.push(crate::relay::RelayOut::UpsertTrigger {
                    id: id.to_string(),
                    name: t.name.clone(),
                    workflow_id: t.workflow_id.clone(),
                    kind: t.kind_tag().into(),
                    enabled: t.enabled,
                    spec: t.spec_value(),
                });
            }
        }
        (frames, mirrors)
    }

    // ----- persistence -----

    pub fn to_value(&self) -> serde_json::Value {
        serde_json::json!({
            "triggers": self.triggers.values().map(|t| serde_json::json!({
                "id": t.id.to_string(),
                "name": t.name,
                "workflow_id": t.workflow_id,
                "kind": t.kind_tag(),
                "spec": t.spec_value(),
                "input": t.input,
                "enabled": t.enabled,
                "catch_up": t.catch_up,
                "last_run": t.last_run.as_ref().map(|(j, ts, st)| serde_json::json!({
                    "job": j, "at": ts, "status": st,
                })),
            })).collect::<Vec<_>>(),
        })
    }

    pub fn from_value(v: &serde_json::Value, mule_tx: Option<std::sync::mpsc::Sender<crate::daemon::mule::MuleJob>>) -> Self {
        let mut s = Scheduler::new(mule_tx);
        if let Some(items) = v.get("triggers").and_then(|x| x.as_array()) {
            for it in items {
                let id = it.get("id").and_then(|x| x.as_str()).and_then(|x| Uuid::parse_str(x).ok());
                let name = it.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let Some(id) = id else { continue };
                if name.is_empty() { continue; }
                let kind = it.get("kind").and_then(|x| x.as_str()).unwrap_or("");
                let spec = it.get("spec").cloned().unwrap_or(serde_json::Value::Null);
                let tk = match kind {
                    "cron" => spec.get("cron").and_then(|x| x.as_str()).map(|e| TriggerKind::Cron { expr: e.into() }),
                    "event" => Some(TriggerKind::Event {
                        event: spec.get("event").and_then(|x| x.as_str()).unwrap_or("").into(),
                        filter: spec.get("filter").cloned().unwrap_or(serde_json::Value::Null),
                    }),
                    "webhook" => Some(TriggerKind::Webhook {
                        source: spec.get("source").and_then(|x| x.as_str()).unwrap_or("").into(),
                        event_tag: spec.get("event").and_then(|x| x.as_str()).map(String::from),
                    }),
                    _ => None,
                };
                let Some(kind) = tk else { continue };
                let last_run = it.get("last_run").and_then(|lr| {
                    Some((
                        lr.get("job")?.as_str()?.to_string(),
                        lr.get("at")?.as_i64()?,
                        lr.get("status")?.as_str()?.to_string(),
                    ))
                });
                s.triggers.insert(id, Trigger {
                    id,
                    name,
                    workflow_id: it.get("workflow_id").and_then(|x| x.as_str()).unwrap_or("").into(),
                    kind,
                    input: it.get("input").cloned().filter(|x| !x.is_null()),
                    enabled: it.get("enabled").and_then(|x| x.as_bool()).unwrap_or(true),
                    catch_up: it.get("catch_up").and_then(|x| x.as_bool()).unwrap_or(false),
                    last_run,
                });
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cron_field_matching() {
        assert!(field_matches("*", 0, 0, 59));
        assert!(field_matches("*", 59, 0, 59));
        assert!(field_matches("*/15", 0, 0, 59));
        assert!(field_matches("*/15", 45, 0, 59));
        assert!(!field_matches("*/15", 20, 0, 59));
        assert!(field_matches("1-5", 3, 0, 59));
        assert!(!field_matches("1-5", 6, 0, 59));
        assert!(field_matches("1,3,5", 3, 0, 59));
        assert!(!field_matches("1,3,5", 4, 0, 59));
        assert!(field_matches("5", 5, 0, 59));
    }

    #[test]
    fn cron_expression_matches() {
        // 1970-01-01 00:00 UTC = thursday (dow 4); pick a known ts:
        // 2026-09-13 03:00:00 UTC is a Sunday
        let ts = 1789268400; // 2026-09-13T03:00:00Z (sunday 03:00)
        assert!(cron_matches("0 3 * * *", ts));
        assert!(cron_matches("0 3 * * 0", ts)); // sunday
        assert!(!cron_matches("0 3 * * 1", ts)); // not monday
        assert!(!cron_matches("5 3 * * *", ts)); // wrong minute
        // every minute
        assert!(cron_matches("* * * * *", ts));
        // hourly at :15
        assert!(!cron_matches("15 * * * *", ts));
        assert!(cron_matches("15 * * * *", ts + 15 * 60));
    }

    #[test]
    fn cron_next_and_prev() {
        let ts = 1789268400; // 03:00 sunday
        // next daily 03:00 = tomorrow
        let next = cron_next("0 3 * * *", ts).unwrap();
        assert_eq!(next, ts + 86_400);
        // next every-5-min = 03:05
        assert_eq!(cron_next("*/5 * * * *", ts).unwrap(), ts + 300);
        // prev daily 03:00 from 03:00 is itself
        assert_eq!(cron_prev("0 3 * * *", ts).unwrap(), ts);
        // prev from 03:01 is 03:00
        assert_eq!(cron_prev("0 3 * * *", ts + 60).unwrap(), ts);
    }

    #[test]
    fn event_matching_with_filters() {
        let mut s = Scheduler::new(None);
        let id = Uuid::new_v4();
        s.triggers.insert(id, Trigger {
            id,
            name: "chain".into(),
            workflow_id: "wf1".into(),
            kind: TriggerKind::Event {
                event: "workflow_completed".into(),
                filter: json!({ "workflow_id": "wf0" }),
            },
            input: Some(json!({ "branch": "main", "summary": "{event.output.summary}" })),
            enabled: true,
            catch_up: false,
            last_run: None,
        });

        // non-matching event type
        let (f, _) = s.fire_event("agent_turn_ended", &json!({}));
        assert!(f.is_empty());
        // matching type, wrong payload
        let (f, _) = s.fire_event("workflow_completed", &json!({ "workflow_id": "other" }));
        assert!(f.is_empty());
        // hit
        let (f, m) = s.fire_event("workflow_completed", &json!({ "workflow_id": "wf0", "output": { "summary": "did the thing" } }));
        assert_eq!(f.len(), 1);
        assert_eq!(m.len(), 1);
        match &f[0] {
            Frame::TriggerFired { trigger, job } => {
                assert_eq!(trigger, &id.to_string());
                assert_eq!(job.len(), 36); // uuid
            }
            _ => panic!("wrong frame"),
        }
        // disabled triggers don't fire
        s.triggers.get_mut(&id).unwrap().enabled = false;
        let (f, _) = s.fire_event("workflow_completed", &json!({ "workflow_id": "wf0" }));
        assert!(f.is_empty());
    }

    #[test]
    fn webhook_matching() {
        let mut s = Scheduler::new(None);
        let id = Uuid::new_v4();
        s.triggers.insert(id, Trigger {
            id,
            name: "gh-push".into(),
            workflow_id: "wf2".into(),
            kind: TriggerKind::Webhook { source: "github".into(), event_tag: Some("push".into()) },
            input: Some(json!({ "ref": "{event.payload.ref}" })),
            enabled: true,
            catch_up: false,
            last_run: None,
        });
        // hit
        let (f, _) = s.fire_event("webhook", &json!({ "source": "github", "event": "push", "payload": { "ref": "refs/heads/main" } }));
        assert_eq!(f.len(), 1);
        // wrong tag
        let (f, _) = s.fire_event("webhook", &json!({ "source": "github", "event": "issues" }));
        assert!(f.is_empty());
        // wrong source
        let (f, _) = s.fire_event("webhook", &json!({ "source": "ci", "event": "push" }));
        assert!(f.is_empty());
    }

    #[test]
    fn input_templating() {
        let t = Trigger {
            id: Uuid::new_v4(),
            name: "t".into(),
            workflow_id: "wf".into(),
            kind: TriggerKind::Event { event: "x".into(), filter: json!({}) },
            input: Some(json!({ "branch": "{event.ref}", "fixed": "main", "n": 3 })),
            enabled: true,
            catch_up: false,
            last_run: None,
        };
        let out = t.job_input(Some(&json!({ "ref": "refs/heads/dev" }))).unwrap();
        assert_eq!(out["branch"], "refs/heads/dev");
        assert_eq!(out["fixed"], "main");
        assert_eq!(out["n"], 3);
        // unresolved template keeps the literal
        let out = t.job_input(Some(&json!({}))).unwrap();
        assert_eq!(out["branch"], "{event.ref}");
    }

    #[test]
    fn scheduler_persists() {
        let mut s = Scheduler::new(None);
        let id = Uuid::new_v4();
        s.triggers.insert(id, Trigger {
            id,
            name: "nightly".into(),
            workflow_id: "wf9".into(),
            kind: TriggerKind::Cron { expr: "0 3 * * *".into() },
            input: Some(json!({"branch": "main"})),
            enabled: true,
            catch_up: true,
            last_run: Some(("job1".into(), 1789268400, "completed".into())),
        });
        let v = s.to_value();
        let back = Scheduler::from_value(&v, None);
        let t = back.triggers.get(&id).unwrap();
        assert_eq!(t.name, "nightly");
        assert!(t.catch_up);
        assert_eq!(t.last_run.as_ref().unwrap().0, "job1");
        match &t.kind {
            TriggerKind::Cron { expr } => assert_eq!(expr, "0 3 * * *"),
            _ => panic!("wrong kind"),
        }
    }
}
