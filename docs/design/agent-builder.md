# Design: The Agent Builder (forge configuration in ranch)

*Create and configure forge agents — profiles, models, skills,
prompts — from any ranch surface. Ranch is the front-end; forge is the
source of truth.*

Status: design (Phase B of PLAN.md).

## 1. Division of responsibility

```
┌───────────────── ranch clients ─────────────────┐
│ TUI :agents overlay    Web Agents page    Mobile Agents screen │
└───────────┬─────────────────────────────────────┘
            │ ProfileList / ProfileGet / ProfilePut / ProfileDelete
            │ SkillList / SkillGet          (ranch frames)
┌───────────▼──────────── ranchd ─────────────────┐
│ forge.rs worker (existing thread + pipe pair)   │
│   new ForgeJob kinds: ProfileList/Get/Put/Delete│
│   SkillList/Get                                 │
└───────────┬─────────────────────────────────────┘
            │ X-API-Key (daemon.toml only)
   ┌────────▼────────────────────────── forge ───────────────┐
   │ POST/GET/PATCH/DELETE /profiles   (api/profiles.rs)     │
   │ GET /v1/models/catalog            (api/openai.rs)       │
   │ GET /sessions …                   (existing integration)│
   │ F3: POST /sessions/{id}/notify    (callbacks, Phase A)  │
   │ F4: recipes CRUD                  (future)              │
   └─────────────────────────────────────────────────────────┘
```

Forge profile credentials never leave the daemon: `ProfilePut` accepts
an `api_key` (write-only); every read returns forge's redacted view
(`[REDACTED_SECRET]`, `db/mod.rs` Profile serialization).

## 2. The profile form (what a human fills in)

Maps 1:1 to forge's `profiles` table (migration 001) + pi config:

| field | source | notes |
|---|---|---|
| name, description | profiles | name unique |
| provider, model | profiles | model picker from `GET /v1/models/catalog` (same catalog the per-pane ModelSet uses). Provider is validated against forge's allowlist — `openai, anthropic, proxy-anthropic, proxy, google, gemini, custom` (`api/profiles.rs` ALLOWED_PROVIDERS, mirrored by the DB CHECK in migration 005) — so the builder's provider dropdown should be populated from that list (hardcode the list client-side; it changes rarely and is CHECK-enforced server-side anyway) |
| base_url, api_key | profiles | key write-only; "leave blank to keep existing" on edit |
| working_dir | profiles | directory picker (existing `DirList` frames) |
| git_url, git_ref | profiles | optional clone source |
| nix_shell | profiles | optional shell expression |
| system_prompt | profiles | multiline textarea (TUI: editor buffer) |
| tools | profiles (JSON array) | checkbox set of known pi tools (read, write, edit, bash) + free-form |
| skills | **F4/future** | pi skills attached to the profile; browse + toggle |

## 3. Frames

```
ProfileList {}                            → ProfileListOk {profiles: [{id, name, description, provider, model, working_dir, updated_at}]}
ProfileGet {profile}                      → ProfileGetOk {profile: {...redacted}}
ProfilePut {profile_id?, draft}           → ProfilePutOk {profile_id}    // upsert: no id = create
ProfileDelete {profile}                   → ProfileDeleteOk
SkillList {}                              → SkillListOk {skills: [...]}  // F4; empty until forge ships skills surfacing
```

Worker pattern: identical to `ForgeJob::List` / `ModelList` today —
blocking ureq in the forge worker thread, results broadcast, clients
match on `req_id`.

## 4. Launching an agent from a profile

Existing machinery covers it: `SessionsCreate {kind:"forge"}` binds a
new chat pane to a new forge session (the daemon POSTs `/sessions`
with the configured profile). The builder adds: profile picker in the
TUI `:agent` flow, web "new agent" dialog, and mobile agent-create
sheet — each lists profiles (`ProfileList`) instead of defaulting to
the first profile (`forge_profile_id` stays the fallback).

## 5. Recipes (F4, later)

A recipe = `{name, profile_draft, skills[], workflow_hint?}` — a
shareable "kind of agent" (e.g. "rust-reviewer", "docs-writer").
Phase 1 ships recipes as **ranch-local JSON**
(`~/.config/ranch/recipes/*.json`) rendered in the new-agent wizard;
when forge lands recipe storage, the same frames proxy to it. Ranch
never needs to be the system of record — it's the editor and the
launcher.

## 6. Acceptance

1. On the phone: create profile "docs-writer" (provider/model/prompt/
   tools) → it appears in the TUI `:agent` profile picker → launching
   it produces a forge session with that model + system prompt (verify
   via forge `GET /sessions/{id}`).
2. Editing a profile's model changes new sessions; in-flight panes
   switch via the existing per-pane ModelSet.
3. API key set once is never displayed again anywhere.

## 7. Web-auth tool + specialist-dot provisioning (H6.5)

An agent that needs a site login (billing, SaaS admin, …) must get
the session credential **without the password ever reaching the
model**. H6.5 ships that as a two-sided broker: the forge side mints
the handoff, the ranch side owns the consent, and the secret store
sits in between.

### 7.1 The flow

```
model                     forge                          user's browser / ranch card
  │ web_login(url, slot)      │
  ├──────────────────────────►│ POST /sessions/:id/web-login
  │                           │   → RanchToolQueue kind "web_login"
  │                           │   → ranch_tool_request card {url, slot} ONLY
  │                           │                        ┌──────────────────────► ranch clients
  │                           │                        │ "Sign me in" / "Cancel" card
  │                           │◄───────────────────────┤ POST /ranch-tools/:id/result
  │                           │  (approval, 60 s TTL)  │
  │                           │  mint one-time token (30 min, single-use)
  │                           │  xdg-open GET /auth/browser-handoff?token=…&next=url
  │  {status:"approved",      │   user: signs in to the site THEMSELVES,
  │   handoff_url}            │   pastes the session cookie into the handoff page
  │◄──────────────────────────┤   POST /auth/browser-handoff (token = credential;
  │                           │   no API key) → secrets(owner, slot, value, valid_until)
  │ web_login_done(slot)      │
  ├──────────────────────────►│ GET /sessions/:id/web-login?slot=…
  │ {status:"signed_in"}      │
  │  → sandbox bash runs with $<slot> (resolve_credential_env at
  │    exec time; expired/missing slot = fail-closed refusal naming
  │    the slot, never the value)
```

### 7.2 The invariants (tested)

- **No credential material crosses any boundary the model can read:**
  the card payload is `{url, slot}`; the harness tool outputs are
  status strings; the handoff URL is a one-time token, not the
  secret. Grep audit: `tests/herd_h65_tests.rs` asserts the cookie
  is absent from every `messages` row, card payload, and response
  after a full round-trip. (harness: `test/web-login.test.ts`.)
- **Fail-closed credentials:** `resolve_credential_env` (forge
  `credentials.rs`) resolves the agent's
  `credentials_scope.env_refs` against the `secrets` table at
  tool-execution time — present / absent / expired are distinct
  results, and the refusal message names the SLOT, never a value.
  Values ride only the container env (`ContainerEnv.extra_env` →
  nspawn `--setenv`), both the streaming and non-streaming bash
  paths (single source of truth: `nspawn_args`).
- **Consent is per-request:** the approval card IS the consent; a
  crash-recovered rerun asks again (the tools are not replay-safe).

### 7.3 Specialist-dot provisioning (`ranch agents new`)

The wizard (CLI) and the mobile ＋Agent form (the `AgentProvision`
frame) run the SAME core (`crates/ranch/src/agents_new.rs` →
`provision()`): `POST /agents` (name, avatar, primary profile,
memory scope + org label, home machine, tools allowlist) + optional
mule cron wake (`target = {forge_conversation: {forge_url, api_key,
agent_id}}` — the H3.3 wake shape; "07:00" → "0 7 * * *", the
5-field standard cron the mule engine parses). The agent row always
lands first; the wake is best-effort (mule unconfigured ⇒ skipped
with a warning, never a failed request). Empty `home_machine` on the
frame = the daemon's own relay machine id (the mobile form's "this
machine"). The credential scope is deliberately NOT part of the
request: slots are declared later via `PATCH /agents/:id
(credentials_scope)` and filled through the §7.1 flow.

### 7.4 v1 limits (documented, not bugs)

- One handoff per approval; the token is single-use, 30 min. No
  refresh loop: an expired slot means the user re-approves +
  re-pastes.
- The handoff page is served for ANY host header (the token IS the
  credential): keep forge loopback- or tunnel-gated, as always.
- Mobile form v1: raw profile id + "all tools". **v2 (2026-10-12, shipped): the
  mobile NewAgentActivity is now the full builder** — profile picker
  (ProfileList), inline new-profile form (ModelCatalog + ProfilePut),
  and the tools allowlist checkbox set (the daemon's KNOWN_TOOLS;
  unchecked = all, the H1.1 empty-allowlist convention).
- `xdg-open` is best-effort: on headless boxes the `handoff_url`
  still comes back in the tool output so the model can relay it.

