//! `ranch agents new` — Herd H6.5 specialist-dot provisioning wizard.
//!
//! Interactive CLI (line-based prompts; defaults shown in [brackets],
//! Enter accepts):
//!
//! ```text
//! name            "Atlas"
//! avatar          (blank = none)
//! profile         pick from `GET {forge}/profiles` (blank = none)
//! tools allowlist comma-separated; blank = allow all (the H1.1 column)
//! memory scope    agent | org   (org → org label)
//! home machine    default: this machine's relay machine_id
//! default wake    "07:00" (daily) or a 5-field cron expr; blank = none
//! ```
//!
//! Writes, in order:
//!  1. `POST {forge}/agents` (the H1.1 row; tenancy = the operator's
//!     `forge_api_key` from daemon.toml — the wizard runs client-side,
//!     so it reuses the same config as every other forge call);
//!  2. `POST {mule}/api/v1/wakes` — one cron wake,
//!     `target = {forge_conversation: {forge_url, api_key, agent_id}}`
//!     (the H3.3 wake shape; the wake engine posts the rendered prompt
//!     to the agent's conversation each due time). Skipped with a
//!     warning when mule is not configured — the agent row still lands.
//!
//! The credential scope (env_refs) is NOT set here in v1: slots are
//! declared later by PATCHing `agents.credentials_scope` and the user
//! signs in through the `web_login` card (the forge half of H6.5).
//! The wizard prints that pointer so nobody is surprised.

use std::io::{BufRead, Write};

use ranch_protocol::{AgentInfo, SecretSlot, WakeInfo};
use serde_json::{Value, json};

use crate::daemon::mule::load_mule_config;
use crate::forge::{ForgeConfig, load_forge_config};

/// The tool names the wizard offers as allowlist suggestions. The
/// allowlist itself accepts any string (forward-compatible with new
/// harness tools); this list is for the prompt hint only.
const KNOWN_TOOLS: &[&str] = &[
    "bash",
    "read",
    "write",
    "webfetch",
    "search",
    "web_login",
    "web_login_done",
    "spawn_subagent",
    "memory_remember",
    "agent_signal",
    "schedule_reminder",
];

// ---------- pure, unit-tested helpers ----------

/// Validate a display name: non-empty after trim, ≤ 64 chars, no
/// newlines (it lands in the agents table and on mobile lists).
pub fn validate_agent_name(name: &str) -> Result<(), String> {
    let t = name.trim();
    if t.is_empty() {
        return Err("name must not be empty".into());
    }
    if t.len() > 64 {
        return Err("name must be at most 64 characters".into());
    }
    if t.contains('\n') || t.contains('\0') {
        return Err("name must not contain newlines".into());
    }
    Ok(())
}

/// Build the cron expr for a daily time: `"07:00"` → `"0 7 * * *"`
/// (the mule wake engine's 5-field subset: sec min hour day month? NO
/// — mule cron is the standard 5-field `min hour dom month dow`, so a
/// daily time is `M H * * *`).
pub fn cron_from_daily_time(input: &str) -> Result<String, String> {
    let t = input.trim();
    let (h, m) = t
        .split_once(':')
        .ok_or_else(|| format!("'{t}' is not HH:MM (or a 5-field cron expr)"))?;
    let h: u32 = h
        .trim()
        .parse()
        .map_err(|_| format!("hour in '{t}' is not a number"))?;
    let m: u32 = m
        .trim()
        .parse()
        .map_err(|_| format!("minute in '{t}' is not a number"))?;
    if h > 23 {
        return Err(format!("hour {h} out of range 0-23"));
    }
    if m > 59 {
        return Err(format!("minute {m} out of range 0-59"));
    }
    Ok(format!("{m} {h} * * *"))
}

/// Parse the wake-schedule input: either a daily time (`"07:00"`) or a
/// 5-field cron expression (`"0 7 * * 1-5"`). Returns the cron expr.
pub fn parse_wake_schedule(input: &str) -> Result<String, String> {
    let t = input.trim();
    if t.is_empty() {
        return Err("empty schedule".into());
    }
    // HH:MM → daily cron
    if t.contains(':') {
        return cron_from_daily_time(t);
    }
    // 5-field cron passthrough: split, first field must be minute-like
    // (numbers / ranges / steps / commas / *). Reject the 6-field
    // (with seconds) form explicitly — the mule engine's subset is 5.
    let fields: Vec<&str> = t.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!(
            "expected a 5-field cron expression (got {} fields)",
            fields.len()
        ));
    }
    for f in &fields {
        if f.is_empty() {
            return Err("empty cron field".into());
        }
        // Each field: * or comma-separated tokens; a token is
        // "N" / "N-M" / optionally with "/step".
        for tok in f.split(',') {
            let base = tok.split('/').next().unwrap_or(tok);
            if base == "*" {
                continue;
            }
            let nums = base.split('-');
            let parts: Vec<&str> = nums.collect();
            if parts.len() > 2 {
                return Err(format!("bad cron field '{f}'"));
            }
            for p in &parts {
                if !p.bytes().all(|b| b.is_ascii_digit()) || p.is_empty() {
                    return Err(format!("bad cron field '{f}'"));
                }
            }
        }
    }
    Ok(t.to_string())
}

/// Build the mule `POST /api/v1/wakes` body for the default wake.
/// Target shape (H3.3): `{forge_conversation: {forge_url, api_key,
/// agent_id}}` — the forge credentials ride in the target (documented
/// v1 behavior; a shared fleet config is the upgrade path).
pub fn wake_body(
    wake_name: &str,
    agent_id: &str,
    cron_expr: &str,
    prompt: &str,
    forge_url: &str,
    api_key: &str,
) -> Value {
    json!({
        "name": wake_name,
        "kind": "cron",
        "spec": {"expr": cron_expr},
        "target": {
            "forge_conversation": {
                "forge_url": forge_url,
                "api_key": api_key,
                "agent_id": agent_id,
            }
        },
        "prompt_template": prompt,
        "enabled": true,
    })
}

/// Build the forge `POST /agents` body.
pub fn create_agent_body(
    name: &str,
    avatar: &str,
    profile_id: &Option<String>,
    memory_scope: &str,
    org_id: &str,
    home_machine: &str,
    tools_allowlist: &[String],
) -> Value {
    let mut v = json!({
        "name": name,
        "visibility": "private",
        "memory_scope": memory_scope,
        "home_machine": home_machine,
        "tools_allowlist": tools_allowlist,
    });
    if !avatar.trim().is_empty() {
        v["avatar_url"] = Value::String(avatar.trim().to_string());
    }
    if let Some(pid) = profile_id
        && !pid.trim().is_empty()
    {
        v["primary_profile_id"] = Value::String(pid.trim().to_string());
    }
    if memory_scope == "org" && !org_id.trim().is_empty() {
        v["org_id"] = Value::String(org_id.trim().to_string());
    }
    v
}

// ---------- shared provisioning core (CLI wizard + daemon frame) ----------

/// One provisioning request: the forge `POST /agents` fields plus the
/// optional mule cron wake. Shared by the CLI wizard and the
/// `AgentProvision` frame handler (mobile's ＋Agent form).
#[derive(Debug, Clone)]
pub struct ProvisionRequest {
    pub name: String,
    pub avatar: Option<String>,
    pub profile_id: Option<String>,
    /// "agent" | "org" (forge ALLOWED_MEMORY_SCOPES)
    pub memory_scope: String,
    pub org_id: Option<String>,
    pub home_machine: String,
    /// empty = allow all
    pub tools_allowlist: Vec<String>,
    /// 5-field cron expr (daily times are converted client-side)
    pub wake_cron: Option<String>,
    pub wake_prompt: Option<String>,
}

/// The outcome of a successful provision: the agent row always lands;
/// the wake is best-effort (None + a wake requested means mule was
/// missing or rejected — the `wake_error` explains why).
#[derive(Debug, Clone)]
pub struct ProvisionOutcome {
    pub agent_id: String,
    pub agent_name: String,
    pub wake_id: Option<String>,
    /// Set when a wake was requested but could not be created.
    pub wake_error: Option<String>,
}

/// Run the provisioning: validate → POST /agents → optional
/// POST /api/v1/wakes. Blocking localhost HTTP (the AgentPause
/// pattern) — callers must already be on a worker/frame thread or
/// the CLI foreground.
pub fn provision(
    cfg: &ForgeConfig,
    mule: Option<&crate::daemon::mule::MuleConfig>,
    req: &ProvisionRequest,
) -> Result<ProvisionOutcome, String> {
    validate_agent_name(&req.name).map_err(|e| format!("name: {e}"))?;
    if req.memory_scope != "agent" && req.memory_scope != "org" {
        return Err(format!(
            "memory_scope must be 'agent' or 'org' (got '{}')",
            req.memory_scope
        ));
    }
    if let Some(expr) = &req.wake_cron {
        parse_wake_schedule(expr).map_err(|e| format!("wake schedule: {e}"))?;
    }
    if req.home_machine.trim().is_empty() {
        return Err("home machine must not be empty".into());
    }

    let body = create_agent_body(
        &req.name,
        req.avatar.as_deref().unwrap_or(""),
        &req.profile_id.clone(),
        &req.memory_scope,
        req.org_id.as_deref().unwrap_or(""),
        &req.home_machine,
        &req.tools_allowlist,
    );
    let created = forge_req(cfg, "POST", "/agents", Some(&body))
        .map_err(|e| format!("creating agent: {e}"))?;
    // forge wraps the agent in {"agent": {...}}
    let agent = created.get("agent").unwrap_or(&created);
    let agent_id = agent
        .get("id")
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .ok_or("forge did not return the agent id")?;

    let mut wake_id = None;
    let mut wake_error = None;
    if let Some(expr) = &req.wake_cron {
        let prompt = req
            .wake_prompt
            .clone()
            .unwrap_or_else(|| format!("Daily check-in for {}.", req.name));
        if let Some(m) = mule {
            let wb = wake_body(
                &format!("wake-{}", req.name),
                &agent_id,
                expr,
                &prompt,
                &cfg.base,
                &cfg.key,
            );
            match post_wake(m, &wb) {
                Ok(w) => wake_id = w.get("id").and_then(|x| x.as_str()).map(str::to_string),
                Err(e) => wake_error = Some(e),
            }
        } else {
            wake_error =
                Some("mule not configured (set mule_url in daemon.toml) — wake skipped".into());
        }
    }

    Ok(ProvisionOutcome {
        agent_id,
        agent_name: req.name.clone(),
        wake_id,
        wake_error,
    })
}

// ---------- HTTP helpers (client-side; mirror forge.rs::http_json) ----------

fn forge_req(
    cfg: &ForgeConfig,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, String> {
    use std::io::Read as _;
    let url = format!("{}{}", cfg.base, path);
    let m = method.to_ascii_uppercase();
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
            .send_json(b.cloned().unwrap_or(Value::Null)),
        ("PATCH", b) => agent
            .patch(&url)
            .header("X-API-Key", &cfg.key)
            .send_json(b.cloned().unwrap_or(Value::Null)),
        ("PUT", b) => agent
            .put(&url)
            .header("X-API-Key", &cfg.key)
            .send_json(b.cloned().unwrap_or(Value::Null)),
        ("DELETE", _) => agent.delete(&url).header("X-API-Key", &cfg.key).call(),
        _ => return Err(format!("unsupported method {m}")),
    };
    let mut res = sent.map_err(|e| format!("forge {m} {path}: {e}"))?;
    let mut text = String::new();
    res.body_mut()
        .as_reader()
        .read_to_string(&mut text)
        .map_err(|e| format!("forge read {path}: {e}"))?;
    if !res.status().is_success() {
        let detail = text.trim();
        return Err(if detail.is_empty() {
            format!("forge {m} {path}: HTTP {}", res.status())
        } else {
            format!("forge {m} {path}: HTTP {} — {detail}", res.status())
        });
    }
    serde_json::from_str(&text).map_err(|e| format!("forge decode {path}: {e}"))
}

// ---------- agent registry edit (H6.5b: mobile ✎ + `ranch agents edit`) ----------

/// One edit request: the forge `PATCH /agents/:id` fields. `None` =
/// "leave the column alone" (forge's partial update treats JSON `null`
/// and an absent key identically, so clearing a nullable column is not
/// supported — documented in the `AgentUpdate` frame docs).
#[derive(Debug, Clone, Default)]
pub struct UpdateRequest {
    pub agent_id: String,
    pub name: Option<String>,
    pub avatar: Option<String>,
    pub profile_id: Option<String>,
    /// "agent" | "org"
    pub memory_scope: Option<String>,
    pub org_id: Option<String>,
    pub home_machine: Option<String>,
    /// "private" | "org"
    pub visibility: Option<String>,
    /// Some(vec![]) = allow all tools
    pub tools_allowlist: Option<Vec<String>>,
    pub extra_instructions: Option<String>,
    /// credential slot NAMES injected into the agent's sandbox (forge
    /// `credentials_scope.env_refs`). Some(..) replaces the set;
    /// Some(vec![]) clears it. (2026-10-18 — the mobile Secrets
    /// section writes the mapping through AgentUpdate.)
    pub env_refs: Option<Vec<String>>,
}

/// Build the forge `PATCH /agents/:id` body: only present, non-blank
/// fields are sent (blank strings are dropped rather than sent as "" —
/// forge cannot distinguish "clear" from "keep" and an empty name would
/// be rejected server-side anyway).
pub fn update_agent_body(req: &UpdateRequest) -> Value {
    let mut v = serde_json::Map::new();
    let mut put = |key: &str, val: &Option<String>| {
        if let Some(s) = val {
            let t = s.trim();
            if !t.is_empty() {
                v.insert(key.to_string(), Value::String(t.to_string()));
            }
        }
    };
    put("name", &req.name);
    put("avatar_url", &req.avatar);
    put("primary_profile_id", &req.profile_id);
    put("memory_scope", &req.memory_scope);
    put("org_id", &req.org_id);
    put("home_machine", &req.home_machine);
    put("visibility", &req.visibility);
    put("extra_instructions", &req.extra_instructions);
    if let Some(tools) = &req.tools_allowlist {
        // Some(vec![]) is meaningful (= allow all), so it is always sent.
        v.insert(
            "tools_allowlist".to_string(),
            Value::Array(
                tools
                    .iter()
                    .map(|t| Value::String(t.trim().to_string()))
                    .collect(),
            ),
        );
    }
    if let Some(refs) = &req.env_refs {
        // Some(vec![]) is meaningful (= inject nothing), so it is
        // always sent. Names are validated (validate_update) before
        // any HTTP — they become sandbox env var names.
        v.insert(
            "credentials_scope".to_string(),
            serde_json::json!({
                "env_refs": refs.iter().map(|r| r.trim().to_uppercase()).collect::<Vec<_>>()
            }),
        );
    }
    Value::Object(v)
}

/// forge's slot-name rule (api/secrets.rs `valid_slot_name`): `^[A-Z]
/// [A-Z0-9_]*$`, ≤ 64 — names become env var names. Duplicated here
/// so the daemon rejects junk before the forge round trip.
pub fn valid_slot_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().enumerate().all(|(i, b)| {
            if i == 0 {
                b.is_ascii_uppercase()
            } else {
                b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'
            }
        })
}

/// Validate an edit request (before any HTTP). At least one field must
/// be present — forge answers 400 "No fields to update".
pub fn validate_update(req: &UpdateRequest) -> Result<(), String> {
    if let Some(n) = &req.name {
        validate_agent_name(n).map_err(|e| format!("name: {e}"))?;
    }
    if let Some(s) = &req.memory_scope
        && s != "agent"
        && s != "org"
    {
        return Err(format!("memory_scope must be 'agent' or 'org' (got '{s}')"));
    }
    if let Some(v) = &req.visibility
        && v != "private"
        && v != "org"
    {
        return Err(format!("visibility must be 'private' or 'org' (got '{v}')"));
    }
    if let Some(refs) = &req.env_refs {
        for r in refs {
            if !valid_slot_name(r.trim()) {
                return Err(format!(
                    "invalid credential slot name '{r}' (want A-Z start, A-Z0-9_ after, ≤ 64)"
                ));
            }
        }
    }
    let body = update_agent_body(req);
    if body.as_object().map(|m| m.is_empty()).unwrap_or(true) {
        return Err("nothing to update".into());
    }
    Ok(())
}

/// PATCH the agent row and return the fresh registry row. Blocking
/// localhost HTTP (the provision/AgentPause pattern).
pub fn update_agent(cfg: &ForgeConfig, req: &UpdateRequest) -> Result<AgentInfo, String> {
    validate_update(req)?;
    let body = update_agent_body(req);
    let patched = forge_req(
        cfg,
        "PATCH",
        &format!("/agents/{}", req.agent_id),
        Some(&body),
    )
    .map_err(|e| format!("updating agent: {e}"))?;
    let agent = patched.get("agent").unwrap_or(&patched);
    agent_info_from_value(agent).ok_or_else(|| "forge did not return the agent row".to_string())
}

/// Decode a forge `agents` row into the wire `AgentInfo`. Shared by
/// `ForgeJob::AgentList` (GET /agents), the update reply (PATCH), and
/// the CLI edit wizard (GET /agents/:id) so every surface prefills the
/// same fields.
pub fn agent_info_from_value(a: &Value) -> Option<AgentInfo> {
    let opt_str = |k: &str| a.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let tools = a
        .get("tools_allowlist")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        });
    Some(AgentInfo {
        id: a.get("id")?.as_str()?.to_string(),
        name: a
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string(),
        avatar_url: opt_str("avatar_url"),
        home_machine: opt_str("home_machine"),
        // TODO(H5): derive from the agent's most-active conversation
        // (GET /agents/:id/conversations?latest=1 per agent — not worth
        // N round trips on a list call).
        last_active: None,
        conversation_count: None,
        primary_profile: opt_str("primary_profile_id"),
        memory_scope: opt_str("memory_scope"),
        org_id: opt_str("org_id"),
        visibility: opt_str("visibility"),
        tools_allowlist: tools,
        extra_instructions: opt_str("extra_instructions"),
        paused: a.get("paused").and_then(|x| x.as_bool()),
        env_refs: a
            .get("credentials_scope")
            .and_then(|c| c.get("env_refs"))
            .and_then(|e| e.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            }),
    })
}

/// Fetch one agent row (GET /agents/:id) — the CLI edit wizard's
/// prefill source.
pub fn get_agent(cfg: &ForgeConfig, agent_id: &str) -> Result<AgentInfo, String> {
    let v = forge_req(cfg, "GET", &format!("/agents/{agent_id}"), None)
        .map_err(|e| format!("reading agent: {e}"))?;
    let agent = v.get("agent").unwrap_or(&v);
    agent_info_from_value(agent).ok_or_else(|| "forge returned no agent row".to_string())
}

/// Resolve a registry name to its id (case-insensitive name match,
/// falling back to "the caller already passed an id").
pub fn resolve_agent_id(cfg: &ForgeConfig, name_or_id: &str) -> Result<String, String> {
    let want = name_or_id.trim();
    if want.is_empty() {
        return Err("agent name or id must not be empty".into());
    }
    let v = forge_req(cfg, "GET", "/agents", None).map_err(|e| format!("listing agents: {e}"))?;
    let arr = v
        .get("agents")
        .and_then(|x| x.as_array())
        .cloned()
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();
    let rows: Vec<AgentInfo> = arr.iter().filter_map(agent_info_from_value).collect();
    if let Some(hit) = rows.iter().find(|a| a.name.eq_ignore_ascii_case(want)) {
        return Ok(hit.id.clone());
    }
    // no name match — assume the caller passed an id (the frames take
    // ids; the CLI takes either)
    Ok(want.to_string())
}

// ---------- wake CRUD (H6.5b: the editor's schedule section) ----------

/// One wake create/update request (the editor's schedule row).
#[derive(Debug, Clone)]
pub struct WakeRequest {
    /// None = create
    pub wake_id: Option<String>,
    pub agent_id: String,
    /// 5-field cron expr (HH:MM inputs are converted by the caller)
    pub cron: String,
    pub prompt: String,
    pub name: Option<String>,
    /// None on create = enabled; Some(b) = the toggle
    pub enabled: Option<bool>,
}

/// The `PATCH /api/v1/wakes/{id}` body (only present keys are sent —
/// mule applies the patch key-by-key).
pub fn wake_update_body(req: &WakeRequest) -> Value {
    let mut v = serde_json::Map::new();
    if let Some(n) = &req.name {
        let t = n.trim();
        if !t.is_empty() {
            v.insert("name".into(), Value::String(t.to_string()));
        }
    }
    if !req.cron.trim().is_empty() {
        v.insert("spec".into(), json!({"expr": req.cron.trim()}));
    }
    if !req.prompt.trim().is_empty() {
        v.insert(
            "prompt_template".into(),
            Value::String(req.prompt.trim().to_string()),
        );
    }
    if let Some(e) = req.enabled {
        v.insert("enabled".into(), Value::Bool(e));
    }
    Value::Object(v)
}

/// Decode mule's `{"wakes":[…]}` (or a bare array) into the editor's
/// wake rows, keeping only the ones bound to `agent_id`
/// (`target.agent_id`, the H3.3 wake shape).
pub fn wakes_for_agent(v: &Value, agent_id: &str) -> Vec<WakeInfo> {
    let arr = v
        .get("wakes")
        .and_then(|x| x.as_array())
        .cloned()
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();
    arr.iter()
        .filter(|w| wake_belongs_to(w, agent_id))
        .filter_map(|w| {
            let id = w.get("id")?.as_str()?.to_string();
            let opt_str = |k: &str| w.get(k).and_then(|x| x.as_str()).map(str::to_string);
            Some(WakeInfo {
                id,
                name: w
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                kind: w
                    .get("kind")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                cron: w
                    .get("spec")
                    .and_then(|s| s.get("expr"))
                    .and_then(|e| e.as_str())
                    .map(str::to_string),
                prompt: w
                    .get("prompt_template")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                enabled: w.get("enabled").and_then(|x| x.as_bool()).unwrap_or(true),
                last_fired_at: opt_str("last_fired_at"),
                next_due_at: opt_str("next_due_at"),
            })
        })
        .collect()
}

/// Does one wake row belong to this agent? The provisioning core
/// writes `target.forge_conversation.agent_id` (the H3.3 shape, and
/// the path mule's own bulk-pause queries); hand-made wakes may carry
/// `target.agent_id` or, for the memory / agent_signal kinds,
/// `spec.agent_id`. Match any of them so the editor never hides a
/// wake the agent will actually hear.
fn wake_belongs_to(w: &Value, agent_id: &str) -> bool {
    [
        w.get("target").and_then(|t| t.get("agent_id")),
        w.get("target")
            .and_then(|t| t.get("forge_conversation"))
            .and_then(|fc| fc.get("agent_id")),
        w.get("spec").and_then(|s| s.get("agent_id")),
    ]
    .into_iter()
    .flatten()
    .any(|a| a.as_str() == Some(agent_id))
}

/// List the agent's wakes (mule `GET /api/v1/wakes`, filtered).
pub fn list_wakes(
    mule: &crate::daemon::mule::MuleConfig,
    agent_id: &str,
) -> Result<Vec<WakeInfo>, String> {
    let v = mule_req(mule, "GET", "/api/v1/wakes", None)?;
    Ok(wakes_for_agent(&v, agent_id))
}

/// Create or update one wake; returns its id.
pub fn put_wake(
    mule: &crate::daemon::mule::MuleConfig,
    forge: &ForgeConfig,
    req: &WakeRequest,
) -> Result<String, String> {
    if req.agent_id.trim().is_empty() {
        return Err("agent_id must not be empty".into());
    }
    if req.prompt.trim().is_empty() {
        return Err("wake prompt must not be empty".into());
    }
    // A blank cron means "leave the schedule alone" on an UPDATE (the
    // editor pauses/resumes a wake without touching its spec — and
    // non-cron kinds have no expr at all); a CREATE needs one.
    if req.wake_id.is_none() && req.cron.trim().is_empty() {
        return Err("a new wake needs a schedule (HH:MM or 5-field cron)".into());
    }
    if !req.cron.trim().is_empty() {
        parse_wake_schedule(&req.cron).map_err(|e| format!("wake schedule: {e}"))?;
    }
    let (method, path, body) = match &req.wake_id {
        Some(id) => (
            "PATCH",
            format!("/api/v1/wakes/{id}"),
            wake_update_body(req),
        ),
        None => (
            "POST",
            "/api/v1/wakes".to_string(),
            wake_body(
                &req.name
                    .clone()
                    .filter(|n| !n.trim().is_empty())
                    .unwrap_or_else(|| format!("wake-{}", req.agent_id)),
                &req.agent_id,
                req.cron.trim(),
                req.prompt.trim(),
                &forge.base,
                &forge.key,
            ),
        ),
    };
    // create honours the enabled toggle too (wake_body defaults true)
    let body = match (req.wake_id.as_deref(), req.enabled) {
        (None, Some(false)) => {
            let mut o = body.as_object().cloned().unwrap_or_default();
            o.insert("enabled".into(), Value::Bool(false));
            Value::Object(o)
        }
        _ => body,
    };
    let v = mule_req(mule, method, &path, Some(&body))?;
    v.get("id")
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .ok_or_else(|| "mule did not return the wake id".to_string())
}

/// Delete one wake.
pub fn delete_wake(mule: &crate::daemon::mule::MuleConfig, wake_id: &str) -> Result<(), String> {
    mule_req(mule, "DELETE", &format!("/api/v1/wakes/{wake_id}"), None)?;
    Ok(())
}

/// mule wakes HTTP (the post_wake shape, widened to the methods the
/// editor needs). Blocking localhost HTTP.
fn mule_req(
    cfg: &crate::daemon::mule::MuleConfig,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, String> {
    use std::io::Read as _;
    let url = format!("{}{}", cfg.wakes_url, path);
    let m = method.to_ascii_uppercase();
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build(),
    );
    let auth = |k: &Option<String>| k.as_ref().map(|k| format!("Bearer {k}"));
    let sent = match (m.as_str(), body) {
        ("GET", _) => {
            let mut r = agent.get(&url);
            if let Some(h) = auth(&cfg.wakes_api_key) {
                r = r.header("Authorization", &h);
            }
            r.call()
        }
        ("DELETE", _) => {
            let mut r = agent.delete(&url);
            if let Some(h) = auth(&cfg.wakes_api_key) {
                r = r.header("Authorization", &h);
            }
            r.call()
        }
        ("POST", Some(b)) => {
            let mut r = agent.post(&url);
            if let Some(h) = auth(&cfg.wakes_api_key) {
                r = r.header("Authorization", &h);
            }
            r.send_json(b.clone())
        }
        ("PATCH", Some(b)) => {
            let mut r = agent.patch(&url);
            if let Some(h) = auth(&cfg.wakes_api_key) {
                r = r.header("Authorization", &h);
            }
            r.send_json(b.clone())
        }
        _ => return Err(format!("mule: unsupported method {m}")),
    };
    let mut res = sent.map_err(|e| format!("mule {m} {path}: {e}"))?;
    let mut text = String::new();
    res.body_mut()
        .as_reader()
        .read_to_string(&mut text)
        .map_err(|e| format!("mule read {path}: {e}"))?;
    if !res.status().is_success() {
        let detail = text.trim();
        return Err(if detail.is_empty() {
            format!("mule {m} {path}: HTTP {}", res.status())
        } else {
            format!("mule {m} {path}: HTTP {} — {detail}", res.status())
        });
    }
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).map_err(|e| format!("mule decode {path}: {e}"))
}

// ---------- the edit wizard (CLI twin of mobile's ✎ form) ----------

fn run_edit(target: &str) {
    let cfg = load_forge_config().unwrap_or_else(|| {
        die("no forge config (set forge_api_key [+ forge_url] in \
             ~/.config/ranch/daemon.toml — see `ranch register`)")
    });
    if target.is_empty() {
        die("usage: ranch agents edit <name-or-id>");
    }
    let agent_id = match resolve_agent_id(&cfg, target) {
        Ok(id) => id,
        Err(e) => die(&e),
    };
    let current = match get_agent(&cfg, &agent_id) {
        Ok(a) => a,
        Err(e) => die(&e),
    };

    println!("editing \"{}\" ({})", current.name, current.id);
    println!("(Enter keeps the current value shown in [brackets])");
    let name = prompt("name", &current.name);
    let avatar = prompt(
        "avatar (url; blank = none)",
        current.avatar_url.as_deref().unwrap_or(""),
    );

    let profiles: Vec<(String, String)> = match forge_req(&cfg, "GET", "/profiles", None) {
        Ok(v) => v
            .get("profiles")
            .and_then(|x| x.as_array())
            .cloned()
            .or_else(|| v.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|p| {
                let id = p.get("id")?.as_str()?.to_string();
                let nm = p
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                Some((id, nm))
            })
            .collect(),
        Err(e) => {
            eprintln!("warning: could not list profiles ({e}); Enter keeps current");
            Vec::new()
        }
    };
    let current_profile = current.primary_profile.clone().unwrap_or_default();
    let profile = prompt(
        &format!(
            "primary profile (id or name; blank keeps '{}')",
            if current_profile.is_empty() {
                "none".to_string()
            } else {
                profiles
                    .iter()
                    .find(|(id, _)| *id == current_profile)
                    .map(|(_, n)| n.clone())
                    .unwrap_or_else(|| current_profile.clone())
            }
        ),
        "",
    );
    let profile_id = {
        let t = profile.trim();
        if t.is_empty() {
            None
        } else if let Some((id, _)) = profiles.iter().find(|(_, n)| n.eq_ignore_ascii_case(t)) {
            Some(id.clone())
        } else {
            Some(t.to_string())
        }
    };

    let current_tools = current
        .tools_allowlist
        .clone()
        .unwrap_or_default()
        .join(", ");
    // credential slots injected into the agent's sandbox (the mobile
    // Secrets section's CLI twin; `ranch secrets list` shows the
    // owner's slot names to reference here)
    let current_refs = current.env_refs.clone().unwrap_or_default().join(", ");
    let refs = prompt(
        &format!(
            "credential slots (comma-separated ENV_NAMES; '-' = none; blank keeps '{}')",
            if current_refs.is_empty() {
                "none"
            } else {
                &current_refs
            }
        ),
        "",
    );
    let refs_input = refs.trim().to_string();
    let env_refs = if refs_input.is_empty() {
        None
    } else if refs_input == "-" {
        Some(vec![])
    } else {
        Some(
            refs_input
                .split(',')
                .map(|s| s.trim().to_uppercase())
                .filter(|s| !s.is_empty())
                .collect(),
        )
    };
    let tools = prompt(
        &format!(
            "tools allowlist (comma-separated; '*' = all; blank keeps '{}')",
            if current_tools.is_empty() {
                "all"
            } else {
                &current_tools
            }
        ),
        "",
    );
    let tools_input = tools.trim().to_string();
    let tools_allowlist = if tools_input.is_empty() {
        None
    } else if tools_input == "*" {
        Some(vec![])
    } else {
        Some(
            tools_input
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        )
    };

    let scope = prompt(
        &format!(
            "memory scope (agent|org; blank keeps '{}')",
            current
                .memory_scope
                .clone()
                .unwrap_or_else(|| "agent".into())
        ),
        "",
    );
    let memory_scope = match scope.trim() {
        "agent" => Some("agent".to_string()),
        "org" => Some("org".to_string()),
        _ => None,
    };
    let org = prompt(
        "org label (blank keeps current)",
        current.org_id.as_deref().unwrap_or(""),
    );
    let org_id = (!org.trim().is_empty() && org.trim() != current.org_id.as_deref().unwrap_or(""))
        .then(|| org.trim().to_string());
    let home = prompt(
        "home machine (blank keeps current)",
        current.home_machine.as_deref().unwrap_or(""),
    );
    let home_machine = (!home.trim().is_empty()
        && home.trim() != current.home_machine.as_deref().unwrap_or(""))
    .then(|| home.trim().to_string());
    let instr = prompt(
        "extra instructions (blank keeps current)",
        current.extra_instructions.as_deref().unwrap_or(""),
    );
    let extra_instructions = (!instr.trim().is_empty()
        && instr.trim() != current.extra_instructions.as_deref().unwrap_or(""))
    .then(|| instr.trim().to_string());

    let req = UpdateRequest {
        agent_id: current.id.clone(),
        name: (name.trim() != current.name && !name.trim().is_empty()).then(|| name.clone()),
        avatar: (!avatar.trim().is_empty()).then(|| avatar.clone()),
        profile_id,
        memory_scope,
        org_id,
        home_machine,
        visibility: None,
        tools_allowlist,
        extra_instructions,
        env_refs,
    };
    if let Err(e) = validate_update(&req) {
        die(&e);
    }
    let body = update_agent_body(&req);
    println!("\n---\npatching {} with {body}\n---", current.id);
    let confirm = prompt("save?", "y");
    if !confirm.eq_ignore_ascii_case("y") {
        println!("aborted");
        return;
    }
    match update_agent(&cfg, &req) {
        Ok(a) => println!("saved \"{}\" ({})", a.name, a.id),
        Err(e) => die(&e),
    }
}

// ---------- interactive prompt plumbing ----------

/// One stdin line (raw mode not needed — the wizard is line-based).
/// Empty read (EOF/Ctrl-D) or error → exit.
fn read_stdin_line() -> String {
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(n) if n > 0 => {}
        _ => {
            eprintln!("\ninterrupted");
            std::process::exit(1);
        }
    }
    line
}

fn prompt(label: &str, default: &str) -> String {
    let d = if default.is_empty() {
        String::new()
    } else {
        format!(" [{default}]")
    };
    print!("{label}{d}: ");
    let _ = std::io::stdout().flush();
    let line = read_stdin_line();
    let line = line.trim();
    if line.is_empty() {
        default.to_string()
    } else {
        line.to_string()
    }
}

fn prompt_choice(label: &str, options: &[&str], default: usize) -> usize {
    print!("{label}");
    for (i, o) in options.iter().enumerate() {
        let marker = if i == default { "*" } else { " " };
        println!("  {i}) {o}{marker}");
    }
    print!("pick [{}]: ", default);
    let _ = std::io::stdout().flush();
    loop {
        let line = read_stdin_line();
        let line = line.trim();
        if line.is_empty() {
            return default;
        }
        match line.parse::<usize>() {
            Ok(n) if n < options.len() => return n,
            _ => println!("  pick a number 0-{}", options.len() - 1),
        }
    }
}

// ---------- the wizard ----------

/// `ranch agents new` — run the wizard; `ranch agents edit` — edit one
/// registry row; `ranch agents` (anything else) is a usage error.
pub fn run(sub: &[String]) {
    match sub.first().map(|s| s.as_str()) {
        Some("new") => run_new(),
        Some("edit") => match sub.get(1) {
            Some(target) => run_edit(target.trim()),
            None => die("usage: ranch agents edit <name-or-id>"),
        },
        Some(other) => die(&format!(
            "unknown subcommand 'agents {other}' (usage: ranch agents new | \
             ranch agents edit <name-or-id>)"
        )),
        None => die("usage: ranch agents new — provision a specialist dot; \
             ranch agents edit <name-or-id> — edit one"),
    }
}

fn die(msg: &str) -> ! {
    eprintln!("ranch agents: {msg}");
    std::process::exit(1);
}

fn run_new() {
    let cfg = load_forge_config().unwrap_or_else(|| {
        die("no forge config (set forge_api_key [+ forge_url] in \
             ~/.config/ranch/daemon.toml — see `ranch register`)")
    });

    let name = prompt("name", "");
    if let Err(e) = validate_agent_name(&name) {
        die(&e);
    }
    let avatar = prompt("avatar (url or path; blank = none)", "");

    // profile picker
    let profiles: Vec<(String, String)> = match forge_req(&cfg, "GET", "/profiles", None) {
        Ok(v) => v
            .get("profiles")
            .and_then(|x| x.as_array())
            .cloned()
            .or_else(|| v.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|p| {
                let id = p.get("id")?.as_str()?.to_string();
                let nm = p
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                Some((id, nm))
            })
            .collect(),
        Err(e) => {
            eprintln!("warning: could not list profiles ({e}); continuing without a picker");
            Vec::new()
        }
    };
    let default_idx = profiles
        .iter()
        .position(|(id, _)| cfg.profile.as_deref() == Some(id.as_str()))
        .unwrap_or(0);
    let options: Vec<String> = profiles
        .iter()
        .enumerate()
        .map(|(i, (_id, nm))| format!("{nm} (#{i})"))
        .collect();
    let option_refs: Vec<&str> = options.iter().map(|s| s.as_str()).collect();
    // profile picker: blank = none, a number = that profile, or a raw
    // profile id pasted (forward-compatible with machines where the
    // list is empty or the caller knows the id).
    let profile_id: Option<String> = if profiles.is_empty() {
        let p = prompt("primary profile id (blank = none)", "");
        (!p.trim().is_empty()).then(|| p.trim().to_string())
    } else {
        print!("primary profile");
        for (i, o) in option_refs.iter().enumerate() {
            let marker = if i == default_idx { " *" } else { "" };
            println!("  {i}) {o}{marker}");
        }
        println!("   (blank = no primary profile; or paste a profile id)");
        print!("pick [{}]: ", default_idx);
        let _ = std::io::stdout().flush();
        let line = read_stdin_line();
        let line = line.trim();
        if line.is_empty() {
            None
        } else if let Ok(n) = line.parse::<usize>() {
            if n < profiles.len() {
                Some(profiles[n].0.clone())
            } else {
                die(&format!(
                    "no such profile number (0-{}); blank for none",
                    profiles.len() - 1
                ))
            }
        } else {
            Some(line.to_string()) // a raw id
        }
    };

    let tools = prompt(
        &format!(
            "tools allowlist (comma-separated, blank = allow all; known: {})",
            KNOWN_TOOLS.join(", ")
        ),
        "",
    );
    let allowlist: Vec<String> = tools
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    for t in &allowlist {
        if !KNOWN_TOOLS.contains(&t.as_str()) {
            eprintln!("warning: '{t}' is not a known tool name (kept as-is)");
        }
    }

    let scope = prompt_choice("memory scope", &["agent", "org"], 0);
    let memory_scope = if scope == 1 { "org" } else { "agent" };
    let org_id = if scope == 1 {
        prompt("org (label)", "org")
    } else {
        String::new()
    };

    let default_machine = crate::relay::load_config()
        .map(|rc| rc.machine_id.clone())
        .unwrap_or_default();
    let home_machine = prompt("home machine", &default_machine);
    if home_machine.trim().is_empty() {
        die("home machine must not be empty");
    }

    let wake_input = prompt(
        "default wake (HH:MM daily or 5-field cron; blank = none)",
        "",
    );
    let wake = if wake_input.trim().is_empty() {
        None
    } else {
        match parse_wake_schedule(&wake_input) {
            Ok(expr) => Some(expr),
            Err(e) => die(&e),
        }
    };
    let wake_prompt = if wake.is_some() {
        prompt("wake prompt", &format!("Daily check-in for {name}."))
    } else {
        String::new()
    };

    // summary + confirm
    println!("\n---");
    println!("name:        {name}");
    if !avatar.trim().is_empty() {
        println!("avatar:      {avatar}");
    }
    match &profile_id {
        Some(id) => {
            let nm = profiles
                .iter()
                .find(|(i, _)| i == id)
                .map(|(_, n)| n.clone())
                .unwrap_or_default();
            println!("profile:     {nm} ({id})");
        }
        None => println!("profile:     (none)"),
    }
    println!(
        "tools:       {}",
        if allowlist.is_empty() {
            "all (no allowlist)".into()
        } else {
            allowlist.join(", ")
        }
    );
    println!("memory:      {memory_scope}",);
    if scope == 1 {
        println!("  org:       {org_id}");
    }
    println!("home:        {home_machine}");
    match &wake {
        Some(expr) => println!("wake:        {expr} — \"{wake_prompt}\""),
        None => println!("wake:        (none)"),
    }
    println!("---");
    let confirm = prompt("create?", "y");
    if !confirm.eq_ignore_ascii_case("y") {
        println!("aborted");
        return;
    }

    // the write step (shared core: also the AgentProvision handler)
    let req = ProvisionRequest {
        name: name.clone(),
        avatar: (!avatar.trim().is_empty()).then(|| avatar.clone()),
        profile_id: profile_id.clone(),
        memory_scope: memory_scope.to_string(),
        org_id: (!org_id.trim().is_empty()).then(|| org_id.clone()),
        home_machine: home_machine.clone(),
        tools_allowlist: allowlist.clone(),
        wake_cron: wake.clone(),
        wake_prompt: (!wake_prompt.trim().is_empty()).then(|| wake_prompt.clone()),
    };
    let mule_cfg = load_mule_config();
    let outcome = match crate::agents_new::provision(&cfg, mule_cfg.as_ref(), &req) {
        Ok(o) => o,
        Err(e) => die(&e),
    };
    println!(
        "created agent {} (\"{}\")",
        outcome.agent_id, outcome.agent_name
    );
    match (outcome.wake_id, outcome.wake_error) {
        (Some(wid), _) => println!(
            "created wake {wid} ({}) — the agent will hear its wake prompt on schedule",
            req.wake_cron.as_deref().unwrap_or("")
        ),
        (None, Some(e)) => eprintln!("warning: {e}"),
        (None, None) => {}
    }

    println!();
    println!(
        "Next: declare credential slots by PATCH /agents/{} (credentials_scope \
         {{\"env_refs\": [...], \"ttl\"}}), then sign in through the agent's web_login \
         card — the cookie never passes through the model.",
        outcome.agent_id
    );
}

fn post_wake(cfg: &crate::daemon::mule::MuleConfig, body: &Value) -> Result<Value, String> {
    use std::io::Read as _;
    let url = format!("{}/api/v1/wakes", cfg.wakes_url);
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build(),
    );
    let mut r = agent.post(&url);
    if let Some(k) = &cfg.wakes_api_key {
        r = r.header("Authorization", &format!("Bearer {k}"));
    }
    let mut res = r
        .send_json(body)
        .map_err(|e| format!("mule POST /api/v1/wakes: {e}"))?;
    let mut text = String::new();
    res.body_mut()
        .as_reader()
        .read_to_string(&mut text)
        .map_err(|e| format!("mule read: {e}"))?;
    if !res.status().is_success() {
        let detail = text.trim();
        return Err(if detail.is_empty() {
            format!("mule: HTTP {}", res.status())
        } else {
            format!("mule: HTTP {} — {detail}", res.status())
        });
    }
    serde_json::from_str(&text).map_err(|e| format!("mule decode: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_time_to_cron() {
        // 5-field standard cron (min hour dom month dow) — the mule
        // wake engine's subset.
        assert_eq!(cron_from_daily_time("07:00").unwrap(), "0 7 * * *");
        assert_eq!(cron_from_daily_time("23:59").unwrap(), "59 23 * * *");
        assert_eq!(cron_from_daily_time("7:5").unwrap(), "5 7 * * *");
        assert!(cron_from_daily_time("25:00").is_err());
        assert!(cron_from_daily_time("07:60").is_err());
        assert!(cron_from_daily_time("nope").is_err());
    }

    #[test]
    fn passthrough_cron_expr() {
        assert_eq!(parse_wake_schedule("0 7 * * 1-5").unwrap(), "0 7 * * 1-5");
        assert_eq!(parse_wake_schedule("07:00").unwrap(), "0 7 * * *");
        // 6-field (seconds) form is rejected
        assert!(parse_wake_schedule("0 0 7 * * *").is_err());
        // junk fields are rejected
        assert!(parse_wake_schedule("* * * * a,b,c").is_err());
        assert!(parse_wake_schedule("1 2 3 4 5 6").is_err());
    }

    #[test]
    fn agent_name_validation() {
        assert!(validate_agent_name("Atlas").is_ok());
        assert!(validate_agent_name("billing-bot-2").is_ok());
        assert!(validate_agent_name("   ").is_err());
        assert!(validate_agent_name(&"a".repeat(65)).is_err());
        assert!(validate_agent_name("bad\nname").is_err());
    }

    #[test]
    fn create_agent_body_shape() {
        let b = create_agent_body(
            "Atlas",
            "https://x/av.png",
            &Some("p1".into()),
            "org",
            "myorg",
            "machine-42",
            &["bash".to_string(), "webfetch".to_string()],
        );
        assert_eq!(b["name"], "Atlas");
        assert_eq!(b["avatar_url"], "https://x/av.png");
        assert_eq!(b["primary_profile_id"], "p1");
        assert_eq!(b["memory_scope"], "org");
        assert_eq!(b["org_id"], "myorg");
        assert_eq!(b["home_machine"], "machine-42");
        assert_eq!(b["tools_allowlist"][0], "bash");
        // agent scope omits org_id; blank avatar omits avatar_url
        let b2 = create_agent_body("B", "   ", &None, "agent", "", "m1", &[]);
        assert!(b2.get("org_id").is_none());
        assert!(b2.get("avatar_url").is_none());
        assert!(b2.get("primary_profile_id").is_none());
    }

    #[test]
    fn wake_body_shape() {
        let b = wake_body(
            "wake-Atlas",
            "agent-1",
            "0 7 * * *",
            "Daily check-in for Atlas.",
            "http://forge.local",
            "sk-test",
        );
        assert_eq!(b["kind"], "cron");
        assert_eq!(b["spec"]["expr"], "0 7 * * *");
        assert_eq!(b["target"]["forge_conversation"]["agent_id"], "agent-1");
        assert_eq!(
            b["target"]["forge_conversation"]["forge_url"],
            "http://forge.local"
        );
        assert_eq!(b["enabled"], true);
    }

    // ---------- H6.5b: the editor's core ----------

    #[test]
    fn update_body_sends_only_present_fields() {
        let req = UpdateRequest {
            agent_id: "a1".into(),
            name: Some("  Atlas II  ".into()),
            avatar: Some("   ".into()), // blank => dropped
            profile_id: Some("p-9".into()),
            memory_scope: Some("org".into()),
            org_id: None,
            home_machine: None,
            visibility: None,
            tools_allowlist: Some(vec![]), // empty is MEANINGFUL
            extra_instructions: Some("be terse".into()),
            env_refs: None,
        };
        let b = update_agent_body(&req);
        assert_eq!(b["name"], "Atlas II");
        assert!(b.get("avatar_url").is_none());
        assert_eq!(b["primary_profile_id"], "p-9");
        assert_eq!(b["memory_scope"], "org");
        assert!(b.get("org_id").is_none());
        assert!(b.get("home_machine").is_none());
        assert_eq!(b["tools_allowlist"], json!([]));
        assert_eq!(b["extra_instructions"], "be terse");
        // an edit with nothing present is not a patch
        let empty = UpdateRequest {
            agent_id: "a1".into(),
            ..Default::default()
        };
        assert_eq!(update_agent_body(&empty), json!({}));
        assert_eq!(validate_update(&empty).unwrap_err(), "nothing to update");
    }

    #[test]
    fn update_validation() {
        let bad_name = UpdateRequest {
            agent_id: "a1".into(),
            name: Some("".into()),
            ..Default::default()
        };
        assert!(validate_update(&bad_name).unwrap_err().starts_with("name:"));
        let bad_scope = UpdateRequest {
            agent_id: "a1".into(),
            memory_scope: Some("everyone".into()),
            ..Default::default()
        };
        assert!(
            validate_update(&bad_scope)
                .unwrap_err()
                .contains("memory_scope")
        );
        let bad_vis = UpdateRequest {
            agent_id: "a1".into(),
            visibility: Some("public".into()),
            ..Default::default()
        };
        assert!(
            validate_update(&bad_vis)
                .unwrap_err()
                .contains("visibility")
        );
        let ok = UpdateRequest {
            agent_id: "a1".into(),
            home_machine: Some("omarchy".into()),
            tools_allowlist: Some(vec!["bash".into()]),
            ..Default::default()
        };
        assert!(validate_update(&ok).is_ok());
    }

    #[test]
    fn agent_info_decodes_editor_fields() {
        let row = json!({
            "id": "uuid-1",
            "name": "Atlas",
            "avatar_url": null,
            "home_machine": "omarchy",
            "primary_profile_id": "p-1",
            "memory_scope": "org",
            "org_id": "myorg",
            "visibility": "private",
            "tools_allowlist": ["bash", "read"],
            "extra_instructions": "be terse",
            "paused": true
        });
        let a = agent_info_from_value(&row).unwrap();
        assert_eq!(a.id, "uuid-1");
        assert_eq!(a.primary_profile.as_deref(), Some("p-1"));
        assert_eq!(a.memory_scope.as_deref(), Some("org"));
        assert_eq!(a.org_id.as_deref(), Some("myorg"));
        assert_eq!(a.visibility.as_deref(), Some("private"));
        assert_eq!(a.tools_allowlist.as_deref().unwrap(), ["bash", "read"]);
        assert_eq!(a.extra_instructions.as_deref(), Some("be terse"));
        assert_eq!(a.paused, Some(true));
        // an older forge row without the columns still decodes
        let thin = json!({"id": "x", "name": "n"});
        let t = agent_info_from_value(&thin).unwrap();
        assert_eq!(t.primary_profile, None);
        assert_eq!(t.tools_allowlist, None);
        assert_eq!(t.paused, None);
        // no id => no row
        assert!(agent_info_from_value(&json!({"name": "n"})).is_none());
    }

    #[test]
    fn wake_patch_body_only_present_keys() {
        let req = WakeRequest {
            wake_id: Some("w-1".into()),
            agent_id: "a1".into(),
            cron: "30 6 * * 1-5".into(),
            prompt: "Standup.".into(),
            name: Some("   ".into()),
            enabled: Some(false),
        };
        let b = wake_update_body(&req);
        assert_eq!(b["spec"]["expr"], "30 6 * * 1-5");
        assert_eq!(b["prompt_template"], "Standup.");
        assert_eq!(b["enabled"], false);
        assert!(b.get("name").is_none());
        // blank cron/prompt are left alone (mule rejects empty ones)
        let blank = WakeRequest {
            wake_id: Some("w-1".into()),
            agent_id: "a1".into(),
            cron: "".into(),
            prompt: "".into(),
            name: Some("morning".into()),
            enabled: None,
        };
        let b2 = wake_update_body(&blank);
        assert_eq!(b2, json!({"name": "morning"}));
    }

    #[test]
    fn wake_list_filters_by_agent_and_decodes() {
        let v = json!({"wakes": [
            {"id": "w-1", "name": "wake-Atlas", "kind": "cron",
             "spec": {"expr": "0 7 * * *"}, "prompt_template": "Daily check-in for Atlas.",
             "enabled": true, "last_fired_at": "2026-10-17T07:00:00Z",
             "next_due_at": "2026-10-18T07:00:00Z",
             "target": {"forge_conversation": {"agent_id": "a1", "forge_url": "http://f"}}},
            {"id": "w-2", "name": "other", "kind": "cron", "spec": {"expr": "0 8 * * *"},
             "prompt_template": "x", "enabled": false,
             "target": {"forge_conversation": {"agent_id": "a2"}}},
            {"id": "w-3", "name": "memory", "kind": "memory",
             "spec": {"agent_id": "a1", "predicate": "recall(\"x\")"},
             "prompt_template": "belief", "enabled": true, "target": {}},
            {"id": "w-4", "name": "bare", "kind": "event", "spec": {},
             "prompt_template": "e", "enabled": true,
             "target": {"agent_id": "a1"}}
        ]});
        let wakes = wakes_for_agent(&v, "a1");
        let ids: Vec<&str> = wakes.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["w-1", "w-3", "w-4"]);
        assert_eq!(wakes[0].cron.as_deref(), Some("0 7 * * *"));
        assert_eq!(wakes[0].prompt, "Daily check-in for Atlas.");
        assert_eq!(
            wakes[0].next_due_at.as_deref(),
            Some("2026-10-18T07:00:00Z")
        );
        assert_eq!(wakes[1].kind, "memory");
        assert_eq!(wakes[1].cron, None);
        assert!(wakes[2].enabled);
        // a bare array answer shape works too
        let bare = json!([{"id": "w-9", "kind": "cron", "spec": {"expr": "0 1 * * *"},
                           "prompt_template": "p", "enabled": true,
                           "target": {"forge_conversation": {"agent_id": "a1"}}}]);
        assert_eq!(wakes_for_agent(&bare, "a1").len(), 1);
        assert!(wakes_for_agent(&bare, "nope").is_empty());
    }

    #[test]
    fn put_wake_requires_schedule_only_on_create() {
        // unreachable mule: validation errors must surface BEFORE any
        // HTTP (their message, not a connect error).
        let mule = crate::daemon::mule::MuleConfig {
            base: "http://127.0.0.1:1".into(),
            api_key: None,
            wakes_forward: false,
            wakes_url: "http://127.0.0.1:1".into(),
            wakes_api_key: None,
        };
        let forge = ForgeConfig {
            base: "http://127.0.0.1:1".into(),
            key: "sk-test".into(),
            profile: None,
        };
        let create = WakeRequest {
            wake_id: None,
            agent_id: "a1".into(),
            cron: "".into(),
            prompt: "p".into(),
            name: None,
            enabled: None,
        };
        assert!(
            put_wake(&mule, &forge, &create)
                .unwrap_err()
                .contains("needs a schedule")
        );
        // an update (the editor's pause/resume) may omit the cron —
        // non-cron kinds have no expr to send
        let update = WakeRequest {
            wake_id: Some("w-1".into()),
            agent_id: "a1".into(),
            cron: "".into(),
            prompt: "p".into(),
            name: None,
            enabled: Some(false),
        };
        let e = put_wake(&mule, &forge, &update).unwrap_err();
        assert!(
            !e.contains("wake schedule"),
            "update with a blank cron reached cron validation: {e}"
        );
        // a bad cron is still rejected on update
        let bad = WakeRequest {
            wake_id: Some("w-1".into()),
            cron: "every morning".into(),
            ..update
        };
        assert!(
            put_wake(&mule, &forge, &bad)
                .unwrap_err()
                .contains("wake schedule")
        );
    }
}

// ---------- sandbox package governance (2026-10-18) ----------
//
// The mobile agent editor's Sandbox section + `ranch sandbox` CLI
// proxy these through the daemon (Sandbox* frames -> these calls ->
// forge api/sandbox_pkgs.rs). The daemon holds a USER key, so phone
// decisions land as decided_by:"user" on the forge side.

/// Ids interpolate into URL paths — keep them strictly uuid-shaped.
fn uuid_shaped(s: &str) -> bool {
    s.len() == 36
        && s.chars().enumerate().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

pub fn sandbox_packages(cfg: &ForgeConfig, agent_id: &str) -> Result<Value, String> {
    if !uuid_shaped(agent_id) {
        return Err("agent_id must be a uuid".into());
    }
    forge_req(
        cfg,
        "GET",
        &format!("/agents/{agent_id}/sandbox/packages"),
        None,
    )
}

pub fn sandbox_decide(
    cfg: &ForgeConfig,
    request_id: &str,
    approve: bool,
    reason: Option<&str>,
) -> Result<Value, String> {
    if !uuid_shaped(request_id) {
        return Err("request_id must be a uuid".into());
    }
    let body = serde_json::json!({
        "decision": if approve { "approve" } else { "reject" },
        "reason": reason,
    });
    forge_req(
        cfg,
        "POST",
        &format!("/sandbox/requests/{request_id}/decision"),
        Some(&body),
    )
}

pub fn sandbox_reset(cfg: &ForgeConfig, agent_id: &str) -> Result<u64, String> {
    if !uuid_shaped(agent_id) {
        return Err("agent_id must be a uuid".into());
    }
    let v = forge_req(
        cfg,
        "POST",
        &format!("/agents/{agent_id}/sandbox/reset"),
        None,
    )?;
    Ok(v.get("rootfs_removed").and_then(Value::as_u64).unwrap_or(0))
}

#[cfg(test)]
mod sandbox_gov_tests {
    use super::*;

    #[test]
    fn uuid_shaped_gate() {
        assert!(uuid_shaped("67d61b63-4970-4fb2-8aaa-f18da08e27c2"));
        assert!(!uuid_shaped("../../etc/passwd"));
        assert!(!uuid_shaped("67d61b63-4970-4fb2-8aaa-f18da08e27c"));
        assert!(!uuid_shaped("67d61b63-4970-4fb2-8aaa-f18da08e27cz"));
    }
}

// ---------- `ranch sandbox` CLI (TUI-surface parity, 2026-10-18) ----------

/// Entry for `ranch sandbox <agent> [subcommand]` — the terminal twin
/// of the mobile Sandbox section. Talks forge directly (USER key),
/// same calls the Sandbox* daemon frames proxy.
pub fn run_sandbox(args: &[String]) {
    let Some(target) = args.first().map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        die(
            "usage: ranch sandbox <name-or-id> [list | approve <req-id> | \
             reject <req-id> [reason…] | reset | request <pkg> [pkg…] [--reason <why>]]",
        );
    };
    if matches!(target, "help" | "--help" | "-h") {
        println!(
            "usage: ranch sandbox <name-or-id> [subcommand]\n\
             \n\
             (no subcommand or `list') show the agent's sandbox package queue\n\
             \tapprove <request-id>          approve a pending request (persistent install)\n\
             \treject <request-id> [reason]  reject a pending request\n\
             \treset                         wipe rootfs trees (keep files; reinstall approved)\n\
             \trequest <pkg> [pkg…] [--reason <why>]   ask the approver on the agent's behalf"
        );
        return;
    }
    let cfg = load_forge_config().unwrap_or_else(|| {
        die("no forge config (set forge_api_key [+ forge_url] in \
             ~/.config/ranch/daemon.toml — see `ranch register`)")
    });
    let agent_id = match resolve_agent_id(&cfg, target) {
        Ok(id) => id,
        Err(e) => die(&e),
    };
    let name = get_agent(&cfg, &agent_id)
        .map(|a| a.name)
        .unwrap_or_else(|_| agent_id.clone());
    let sub = args.get(1).map(|s| s.as_str()).unwrap_or("list");
    match sub {
        "list" => {
            let data = match sandbox_packages(&cfg, &agent_id) {
                Ok(v) => v,
                Err(e) => die(&e),
            };
            println!(
                "sandbox for {name}  (base ready: {})",
                data.get("base_ready")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            );
            if let Some(appr) = data.get("approver").filter(|v| !v.is_null()) {
                println!(
                    "  approver: {}",
                    appr.get("name").and_then(Value::as_str).unwrap_or("?")
                );
            }
            for (bucket, label) in [
                ("pending", "PENDING"),
                ("approved", "APPROVED (installing)"),
                ("installed", "INSTALLED"),
                ("rejected", "REJECTED"),
            ] {
                let Some(rows) = data.get(bucket).and_then(Value::as_array) else {
                    continue;
                };
                if rows.is_empty() {
                    continue;
                }
                println!("  {label}:");
                for r in rows {
                    let id = r.get("id").and_then(Value::as_str).unwrap_or("?");
                    let pkg = r.get("package").and_then(Value::as_str).unwrap_or("?");
                    let why = r.get("reason").and_then(Value::as_str).unwrap_or("");
                    let dec = r.get("decided_by").and_then(Value::as_str).unwrap_or("");
                    let err = r.get("install_error").and_then(Value::as_str).unwrap_or("");
                    println!(
                        "    {id}  {pkg:<24} {why}{}{}",
                        if dec.is_empty() {
                            String::new()
                        } else {
                            format!(" [{dec}]")
                        },
                        if err.is_empty() {
                            String::new()
                        } else {
                            format!(" ! {}", err.chars().take(60).collect::<String>())
                        }
                    );
                }
            }
            let empty = ["pending", "approved", "installed", "rejected"]
                .iter()
                .all(|k| {
                    data.get(*k)
                        .and_then(Value::as_array)
                        .is_none_or(|a| a.is_empty())
                });
            if empty {
                println!(
                    "  (no packages beyond the base image — the agent can ask for more \
                         with request_sandbox_packages)"
                );
            }
        }
        "approve" | "reject" => {
            let Some(rid) = args.get(2) else {
                die(&format!(
                    "{sub} needs a <request-id> (see `ranch sandbox {name} list`)"
                ));
            };
            let reason = if sub == "reject" {
                let r: Vec<&str> = args[3..].iter().map(|s| s.as_str()).collect();
                if r.is_empty() {
                    None
                } else {
                    Some(r.join(" "))
                }
            } else {
                None
            };
            match sandbox_decide(&cfg, rid, sub == "approve", reason.as_deref()) {
                Ok(v) => println!(
                    "{} `{}`",
                    if sub == "approve" {
                        "Approved"
                    } else {
                        "Rejected"
                    },
                    v.get("package").and_then(Value::as_str).unwrap_or("?")
                ),
                Err(e) => die(&e),
            }
        }
        "reset" => match sandbox_reset(&cfg, &agent_id) {
            Ok(n) => println!(
                "reset {name}: {n} rootfs tree(s) removed \
                              (next bash re-copies a clean base + reinstalls approved packages)"
            ),
            Err(e) => die(&e),
        },
        "request" => {
            let rest = &args[2..];
            let mut pkgs: Vec<String> = Vec::new();
            let mut reason: Option<String> = None;
            let mut i = 0;
            while i < rest.len() {
                if rest[i] == "--reason" {
                    reason = rest.get(i + 1).cloned();
                    i += 2;
                } else {
                    pkgs.push(rest[i].to_ascii_lowercase());
                    i += 1;
                }
            }
            if pkgs.is_empty() {
                die("request needs at least one package name");
            }
            // User-side request: forge accepts it directly (the user
            // credential bypasses the pending queue only via their own
            // decision — same flow, still approvable on the phone).
            let body = serde_json::json!({ "packages": pkgs, "reason": reason });
            match forge_req(
                &cfg,
                "POST",
                &format!("/agents/{agent_id}/sandbox/requests"),
                Some(&body),
            ) {
                Ok(v) => {
                    let req = v.get("requested").and_then(Value::as_array);
                    let skp = v.get("skipped").and_then(Value::as_array);
                    if let Some(rows) = req {
                        for r in rows {
                            println!(
                                "requested `{}` (request {})",
                                r.get("package").and_then(Value::as_str).unwrap_or("?"),
                                r.get("id").and_then(Value::as_str).unwrap_or("?")
                            );
                        }
                    }
                    if let Some(rows) = skp {
                        for r in rows {
                            println!(
                                "skipped `{}`: {}",
                                r.get("package").and_then(Value::as_str).unwrap_or("?"),
                                r.get("why").and_then(Value::as_str).unwrap_or("?")
                            );
                        }
                    }
                }
                Err(e) => die(&e),
            }
        }
        other => die(&format!(
            "unknown subcommand 'sandbox … {other}' \
                     (try: list, approve, reject, reset, request)"
        )),
    }
}

pub fn sandbox_request(
    cfg: &ForgeConfig,
    agent_id: &str,
    packages: &[String],
    reason: Option<&str>,
) -> Result<Value, String> {
    if !uuid_shaped(agent_id) {
        return Err("agent_id must be a uuid".into());
    }
    if packages.is_empty() || packages.len() > 10 {
        return Err("packages must hold 1-10 names".into());
    }
    let body = serde_json::json!({ "packages": packages, "reason": reason });
    forge_req(
        cfg,
        "POST",
        &format!("/agents/{agent_id}/sandbox/requests"),
        Some(&body),
    )
}

// ---------- credential slots + vault picker (2026-10-18) ----------
//
// The mobile editor's Secrets section (Secrets* frames) and
// `ranch secrets` proxy these — the daemon holds the USER key, so
// forge sees the owner (slots are strictly owner-scoped there). The
// secret VALUE never transits any of this: binding a slot to the
// vault stores only the `bw://…` pointer; the value is resolved by
// forge at exec time inside the sandbox.

/// Map one forge `GET /secrets` row into the wire `SecretSlot`.
fn secret_slot_from_value(v: &Value) -> Option<SecretSlot> {
    Some(SecretSlot {
        name: v.get("name")?.as_str()?.to_string(),
        vault: v.get("vault").and_then(Value::as_bool).unwrap_or(false),
        bw_ref: v.get("bw_ref").and_then(|x| x.as_str()).map(str::to_string),
        updated_at: v
            .get("updated_at")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        valid_until: v
            .get("valid_until")
            .and_then(|x| x.as_str())
            .map(str::to_string),
    })
}

/// The caller's slots + the Vaultwarden item NAMES for the picker.
/// A missing/failed vault config degrades to `(slots, [], false)`
/// rather than failing the whole listing (the section still manages
/// literal slots); slot-list failures are hard errors.
pub fn secrets_list(cfg: &ForgeConfig) -> Result<(Vec<SecretSlot>, Vec<String>, bool), String> {
    let v = forge_req(cfg, "GET", "/secrets", None)?;
    let slots = v
        .get("secrets")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(secret_slot_from_value).collect())
        .unwrap_or_default();
    let (items, enabled) = match forge_req(cfg, "GET", "/bitwarden/secrets", None) {
        Ok(v) => (
            v.get("keys")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|k| k.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            true,
        ),
        // 502/403-class (not configured, bw down, or a restricted key
        // on the vault browse) — soft, per the doc comment.
        Err(_) => (Vec::new(), false),
    };
    Ok((slots, items, enabled))
}

/// Create/replace one slot. Exactly one source: `vault_item` binds
/// `bw://notes/<item>` (the picker's choice — one item, one note,
/// one token is the lab convention), `literal` stores the typed
/// value. Mirrors forge's PUT validation client-side first.
pub fn secrets_bind(
    cfg: &ForgeConfig,
    name: &str,
    vault_item: Option<&str>,
    literal: Option<&str>,
) -> Result<SecretSlot, String> {
    let name = name.trim().to_uppercase();
    if !valid_slot_name(&name) {
        return Err(format!(
            "invalid slot name '{name}' (want A-Z start, A-Z0-9_ after, ≤ 64)"
        ));
    }
    let value = match (vault_item, literal) {
        (Some(item), None) => {
            let item = item.trim();
            if item.is_empty() || item.contains("\n") || item.len() > 200 {
                return Err("vault item name is not usable".into());
            }
            format!("bw://notes/{item}")
        }
        (None, Some(v)) => {
            if v.is_empty() {
                return Err("literal value is empty".into());
            }
            v.to_string()
        }
        _ => return Err("exactly one of vault_item / literal must be set".into()),
    };
    let body = serde_json::json!({ "value": value });
    forge_req(cfg, "PUT", &format!("/secrets/{name}"), Some(&body))?;
    // forge's PUT answers {ok:true}; re-read the one row's meta so
    // the client gets the same SecretSlot shape as SecretsList.
    let v = forge_req(cfg, "GET", "/secrets", None)?;
    v.get("secrets")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find(|r| r.get("name").and_then(Value::as_str) == Some(name.as_str()))
        })
        .and_then(secret_slot_from_value)
        .ok_or_else(|| format!("forge accepted the slot but did not list '{name}'"))
}

/// Drop one slot. Agents' env_refs may still name it — forge's
/// credential gating degrades missing slots to "unavailable" (exec
/// refusal + the agent's report), which is the desired loud failure.
pub fn secrets_unbind(cfg: &ForgeConfig, name: &str) -> Result<bool, String> {
    let name = name.trim().to_uppercase();
    if !valid_slot_name(&name) {
        return Err(format!(
            "invalid slot name '{name}' (want A-Z start, A-Z0-9_ after, ≤ 64)"
        ));
    }
    let v = forge_req(cfg, "DELETE", &format!("/secrets/{name}"), None)?;
    Ok(v.get("removed").and_then(Value::as_bool).unwrap_or(false))
}

/// Entry for `ranch secrets [subcommand]` — terminal twin of the
/// mobile Secrets section.
pub fn run_secrets(args: &[String]) {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("list");
    if matches!(sub, "help" | "--help" | "-h") {
        println!(
            "usage: ranch secrets list\n\
             \t  ranch secrets bind <NAME> --vault <item>\n\
             \t  ranch secrets bind <NAME> --literal <value>\n\
             \t  ranch secrets unbind <NAME>\n\
             \n\
             Slots are owner-level credential env vars; agents name them in\n\
             credentials_scope.env_refs (mobile editor: Secrets section).\n\
             A --vault binding stores the pointer bw://notes/<item> — the\n\
             value is fetched from the vault at exec time and never stored."
        );
        return;
    }
    let cfg = load_forge_config().unwrap_or_else(|| {
        die("no forge config (set forge_api_key [+ forge_url] in \
             ~/.config/ranch/daemon.toml — see `ranch register`)")
    });
    match sub {
        "list" => {
            let (slots, items, enabled) = match secrets_list(&cfg) {
                Ok(v) => v,
                Err(e) => die(&e),
            };
            println!("slots:");
            if slots.is_empty() {
                println!("  (none — bind one: ranch secrets bind MY_TOKEN --vault <item>)");
            }
            for s in &slots {
                let src = match &s.bw_ref {
                    Some(r) => r.clone(),
                    None => "•••• (literal)".to_string(),
                };
                let exp = s
                    .valid_until
                    .as_deref()
                    .map(|v| format!(" expires {v}"))
                    .unwrap_or_default();
                println!("  {:<28} {src}{}", s.name, exp);
            }
            println!(
                "vault: {}",
                if enabled {
                    "configured"
                } else {
                    "not configured / unreachable"
                }
            );
            if enabled {
                println!(
                    "  items: {}",
                    if items.is_empty() {
                        "(empty)".into()
                    } else {
                        items.join(", ")
                    }
                );
            }
        }
        "bind" => {
            let Some(name) = args.get(1) else {
                die("bind needs <NAME> and --vault <item> | --literal <value>");
            };
            let mut vault_item = None;
            let mut literal = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--vault" => {
                        vault_item = args.get(i + 1).cloned();
                        i += 2;
                    }
                    "--literal" => {
                        literal = args.get(i + 1).cloned();
                        i += 2;
                    }
                    _ => i += 1,
                }
            }
            match secrets_bind(&cfg, name, vault_item.as_deref(), literal.as_deref()) {
                Ok(s) => println!(
                    "bound {} → {}",
                    s.name,
                    s.bw_ref.unwrap_or_else(|| "•••• (literal)".into())
                ),
                Err(e) => die(&e),
            }
        }
        "unbind" => {
            let Some(name) = args.get(1) else {
                die("unbind needs <NAME>");
            };
            match secrets_unbind(&cfg, name) {
                Ok(true) => println!("removed slot {name}"),
                Ok(false) => println!("no such slot (nothing removed)"),
                Err(e) => die(&e),
            }
        }
        other => die(&format!(
            "unknown subcommand 'secrets … {other}' (try: list, bind, unbind)"
        )),
    }
}

#[cfg(test)]
mod secrets_tests {
    use super::*;

    #[test]
    fn slot_name_validation() {
        assert!(valid_slot_name("GITHUB_TOKEN"));
        assert!(valid_slot_name("A"));
        assert!(valid_slot_name("SLOT_2"));
        assert!(!valid_slot_name("github_token")); // lowercase
        assert!(!valid_slot_name("1BAD")); // digit start
        assert!(!valid_slot_name("HAS-DASH"));
        assert!(!valid_slot_name(&"A".repeat(65)));
    }

    #[test]
    fn update_body_env_refs() {
        let req = UpdateRequest {
            agent_id: "a".into(),
            env_refs: Some(vec!["github_token ".into(), "SENTRY_DSN".into()]),
            ..Default::default()
        };
        let b = update_agent_body(&req);
        assert_eq!(b["credentials_scope"]["env_refs"][0], "GITHUB_TOKEN");
        assert_eq!(b["credentials_scope"]["env_refs"][1], "SENTRY_DSN");
        // clearing is expressible
        let clear = UpdateRequest {
            env_refs: Some(vec![]),
            ..req
        };
        assert_eq!(
            update_agent_body(&clear)["credentials_scope"]["env_refs"],
            json!([])
        );
        // and validate_update gates junk before HTTP
        let junk = UpdateRequest {
            env_refs: Some(vec!["bad-name".into()]),
            ..Default::default()
        };
        assert!(
            validate_update(&junk)
                .unwrap_err()
                .contains("invalid credential slot")
        );
    }

    #[test]
    fn agent_info_env_refs_from_credentials_scope() {
        let row = json!({
            "id": "aa", "name": "dependabot",
            "credentials_scope": {"env_refs": ["GITHUB_TOKEN"]}
        });
        let a = agent_info_from_value(&row).unwrap();
        assert_eq!(
            a.env_refs.as_deref(),
            Some(&["GITHUB_TOKEN".to_string()][..])
        );
        // older forge rows (column absent) → None, not empty
        let row2 = json!({"id": "bb", "name": "x"});
        assert_eq!(agent_info_from_value(&row2).unwrap().env_refs, None);
    }

    #[test]
    fn slot_mapping_and_bind_source() {
        let row = json!({
            "name": "GITHUB_TOKEN", "vault": true,
            "bw_ref": "bw://notes/mule-github-token-lab",
            "updated_at": "2026-10-18T14:00:00Z", "valid_until": null
        });
        let s = secret_slot_from_value(&row).unwrap();
        assert_eq!(s.name, "GITHUB_TOKEN");
        assert!(s.vault);
        assert_eq!(
            s.bw_ref.as_deref(),
            Some("bw://notes/mule-github-token-lab")
        );
        assert_eq!(s.valid_until, None);
        // bind source rules (no HTTP needed — errors before forge_req)
        let cfg = ForgeConfig {
            base: "http://127.0.0.1:1".into(),
            key: "k".into(),
            profile: None,
        };
        assert!(
            secrets_bind(&cfg, "X", None, None)
                .unwrap_err()
                .contains("exactly one")
        );
        assert!(
            secrets_bind(&cfg, "X", Some("i"), Some("v"))
                .unwrap_err()
                .contains("exactly one")
        );
        assert!(
            secrets_bind(&cfg, "bad name", Some("i"), None)
                .unwrap_err()
                .contains("invalid slot name")
        );
        assert!(
            secrets_bind(&cfg, "OK", Some(""), None)
                .unwrap_err()
                .contains("not usable")
        );
    }
}
