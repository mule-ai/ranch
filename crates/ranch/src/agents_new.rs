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
    let agent = created
        .get("agent")
        .unwrap_or(&created);
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

/// `ranch agents new` — run the wizard; `ranch agents` (anything else)
/// is a usage error.
pub fn run(sub: Option<String>) {
    match sub.as_deref() {
        Some("new") => run_new(),
        Some(other) => die(&format!(
            "unknown subcommand 'agents {other}' (usage: ranch agents new)"
        )),
        None => die("usage: ranch agents new — provision a specialist dot"),
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
}
