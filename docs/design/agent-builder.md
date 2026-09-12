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
