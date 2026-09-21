// Wire types mirroring crates/ranch-protocol/src/lib.rs (serde tag = "t",
// variant names as-is). Only what the mobile client needs.

export type ForgeSessionInfo = {
  id: string;
  title: string;
  updated: string;
  ended?: string | null;
  working_dir?: string | null;
};

export type PiSessionInfo = {
  id: string;
  title: string;
  session_file: string;
  active: boolean;
  external?: boolean;
  updated?: string;
  path?: string;
};

export type SessionMeta = {
  id: string;
  name: string;
  kind: string; // "shell" | "forge" | "mule"
  active_pane: string;
  panes: string[];
  ref_id?: string;
};

export type Cursor = { x: number; y: number; visible: boolean };

export type PaneSnap = {
  id: string;
  cols: number;
  rows: number;
  lines: string[]; // full screen, row-major, plain text
  seq?: number; // pane update seq at snapshot time (dedup/gap seed)
  cursor?: Cursor;
  kind?: "pty" | "forge-chat";
  chat?: ChatMsg[];
  chat_has_more?: boolean; // true when `chat` is a limited tail (scrollback for older)
  forge_session?: string;
  agentBusy?: boolean;
  model?: string; // active agent model display name (chat panes)
  context?: string; // context-window readout (chat panes)
};

// One row of a forge agent conversation (M8)
export type ChatMsg = {
  seq: number;
  role: string; // "user" | "assistant" | "tool"
  text: string;
  tool_name?: string;
  tool_call_id?: string;
  tool_output?: string;
  duration_ms?: number;
  created_at?: string;
  attachments?: string[];
};


// ----- agent builder (Phase B): forge profile CRUD proxy -----

// ----- agent builder (Phase B): forge profile CRUD proxy -----

// One selectable agent model (pi models.json entry via forge's catalog).
export type ModelChoice = { provider: string; id: string; name: string };

export type ProfileSummary = {
  id: string;
  name: string;
  description?: string | null;
  provider: string;
  model: string;
  working_dir?: string | null;
  updated_at?: string | null;
};

export type Profile = {
  id: string;
  name: string;
  description?: string | null;
  provider: string;
  model: string;
  base_url?: string | null;
  api_key?: string | null; // arrives redacted
  working_dir?: string | null;
  git_url?: string | null;
  git_ref?: string | null;
  nix_shell?: string | null;
  system_prompt: string;
  tools: string[];
  updated_at?: string | null;
};

export type ProfileDraft = {
  name: string;
  description?: string | null;
  provider: string;
  model: string;
  base_url?: string | null;
  api_key?: string | null; // write-only
  working_dir?: string | null;
  git_url?: string | null;
  git_ref?: string | null;
  nix_shell?: string | null;
  system_prompt?: string | null;
  tools: string[];
};

// ----- agent tools (Phase A): agent-spawned panes -----

export type ChatMsgDone = { pane: string; outcome: string; last?: string };

// ----- agent ask (Phase A2): agent asks user questions -----

export type AgentAskRequest = {
  ask_id: string;
  session: string;
  pane: string;
  question: string;
  choices: string[];
  suggested?: number | null;
  multi: boolean;
  free_text: boolean;
};

export type AgentAskAnswer = {
  ask_id: string;
  choices: number[];
  text: string;
};


// ----- workflows (Phase C): mule proxy -----

export type WorkflowSummary = {
  id: string;
  name: string;
  description?: string | null;
  is_async?: boolean | null;
  updated_at?: string | null;
};

export type WorkflowStep = {
  id?: string | null;
  step_order: number;
  type: string; // "agent" | "wasm_module"
  agent_id?: string | null;
  wasm_module_id?: string | null;
  config: unknown;
};

export type WorkflowDraft = {
  name: string;
  description?: string | null;
  is_async: boolean;
  steps: WorkflowStep[];
};

// Binary split tree, mirrors ranch_protocol::Layout

// ----- triggers + webhooks (Phase D/E) -----

export type TriggerRow = {
  id?: string | null;
  name: string;
  workflow_id: string;
  kind: "cron" | "event" | "webhook";
  spec: { cron?: string; tz?: string; event?: string; filter?: Record<string, unknown>; source?: string };
  input?: unknown;
  enabled: boolean;
  catch_up?: boolean;
  last_run?: { job: string; at: number; status: string } | null;
};

export type WebhookRow = {
  id: string;
  name: string;
  sources: string[];
  enabled: boolean;
  created_at?: string | null;
};
export type Layout =
  | { k: "Leaf"; pane: string }
  | { k: "Split"; dir: 0 | 1; pct: number; a: Layout; b: Layout };

export type PaneMeta = {
  pane: string;
  kind: string;
  status?: string;
};

export type Frame =
  | { t: "Hello"; id: string; client: string; caps?: string[] }
  | { t: "HelloOk"; id: string; machine: string; sessions: SessionMeta[]; version?: string | null; monitor_external_pi?: boolean }
  | { t: "Attach"; id: string; client: string; session: string; pane?: string; chat_limit?: number }
  | { t: "Detach"; id: string; client: string }
  | {
      t: "Snapshot";
      id: string;
      client: string;
      session: string;
      seq: number;
      layout: Layout;
      active_pane: string;
      panes: PaneSnap[];
      meta?: PaneMeta[];
    }
  | {
      t: "Update";
      id: string;
      client: string;
      session: string;
      pane: string;
      seq: number;
      cols: number;
      rows: number;
      rows_upd: [number, string][];
      cursor?: Cursor;
      title?: string;
    }
  | { t: "Input"; id: string; client: string; session: string; pane: string; data: string }
  | { t: "Resize"; id: string; client: string; session: string; cols: number; rows: number }
  | { t: "ScrollbackReq"; id: string; client: string; session: string; pane: string; offset: number; limit: number }
  | { t: "Scrollback"; id: string; client: string; session: string; pane: string; offset: number; lines: string[] }
  | { t: "SessionsCreate"; req_id: string; name?: string; kind?: "shell" | "forge" | "pi"; cwd?: string; forge_session?: string; pi_session_file?: string }
  | { t: "ForgeList"; id: string; client: string; req_id: string }
  | { t: "PiList"; id: string; client: string; req_id: string }
  | { t: "DirList"; id: string; client: string; req_id: string; path?: string }
  | { t: "DirListOk"; id: string; req_id: string; path: string; parent?: string | null; dirs: string[]; files?: string[] }
  | { t: "FileRead"; id: string; client: string; req_id: string; path: string }
  | { t: "FileReadOk"; id: string; req_id: string; path: string; content: string; mtime: number; size: number }
  | { t: "FileWrite"; id: string; client: string; req_id: string; path: string; content: string; mtime?: number | null }
  | { t: "FileWriteOk"; id: string; req_id: string; path: string; mtime: number }
  | { t: "FileChanged"; path: string; mtime: number }
  | { t: "ForgeListOk"; id: string; req_id: string; sessions: ForgeSessionInfo[] }
  | { t: "PiListOk"; id: string; req_id: string; sessions: PiSessionInfo[] }
  | { t: "PiMonitor"; enabled: boolean; req_id: string }
  | { t: "PiMonitorOk"; req_id: string; enabled: boolean }
  | { t: "ChatSend"; id: string; client: string; session: string; pane: string; text: string; attachments?: string[] }
  | { t: "ChatCompact"; id: string; client: string; session: string; pane: string; req_id: string }
  | { t: "Chat"; id: string; session: string; pane: string; msgs: ChatMsg[]; reset?: boolean }
  | { t: "ChatHistory"; id: string; client: string; session: string; pane: string; req_id: string; limit: number; before?: number | null }
  | { t: "ChatHistoryOk"; req_id: string; pane: string; msgs: ChatMsg[]; has_more: boolean }
  | { t: "SessionsAck"; req_id: string; session: string; pane: string }
  | { t: "SessionsRename"; session: string; name: string }
  | { t: "SessionsKill"; session: string }
  | { t: "Upgrade" }
  | { t: "SessionsSelect"; session: string; pane: string }
  | { t: "PaneSplit"; req_id: string; session: string; pane: string; dir: 0 | 1 }
  | { t: "PaneResize"; session: string; pane: string; dir: 0 | 1; delta: number }
  | { t: "PaneKill"; session: string; pane: string }
  | { t: "Meta"; id: string; session: string; pane?: string; kind: string; status?: string; preview?: string }
  | { t: "AgentSpawn"; req_id: string; caller_pane: string; caller_session: string; kind: string; profile_id?: string | null; name?: string | null; cwd?: string | null; prompt: string; mode?: string; callback?: boolean }
  | { t: "AgentSpawnOk"; req_id: string; spawn_id: string; session: string; pane: string }
  | { t: "AgentSpawnRequest"; spawn_id: string; caller_pane: string; kind: string; preview: string }
  | { t: "AgentSpawnApprove"; spawn_id: string; allow: boolean }
  | { t: "AgentAskRequest"; ask_id: string; session: string; pane: string; question: string; choices: string[]; suggested?: number | null; multi: boolean; free_text: boolean }
  | { t: "AgentAskAnswer"; ask_id: string; choices: number[]; text: string }
  | { t: "AgentSend"; req_id: string; caller_pane: string; session: string; pane: string; text: string; delivery?: string }
  | { t: "AgentStatus"; req_id: string; caller_pane: string; pane: string }
  | { t: "AgentStatusOk"; req_id: string; pane: string; state: string; model?: string | null }
  | { t: "AgentRead"; req_id: string; caller_pane: string; pane: string; since_seq?: number; limit?: number }
  | { t: "AgentReadOk"; req_id: string; pane: string; msgs: ChatMsg[] }
  | { t: "AgentClose"; req_id: string; caller_pane: string; session: string; pane: string }
  | { t: "AgentDone"; spawn_id: string; session: string; pane: string; outcome: string; last_row?: ChatMsg | null }
  | { t: "ProfileList"; id: string; req_id: string }
  | { t: "ModelCatalog"; id: string; req_id: string }
  | { t: "ModelCatalogOk"; id: string; req_id: string; models: ModelChoice[] }
  | { t: "ProfileListOk"; id: string; req_id: string; profiles: ProfileSummary[] }
  | { t: "ProfileGet"; id: string; req_id: string; profile: string }
  | { t: "ProfileGetOk"; id: string; req_id: string; profile: Profile }
  | { t: "ProfilePut"; id: string; req_id: string; profile_id?: string | null; draft: ProfileDraft }
  | { t: "ProfilePutOk"; id: string; req_id: string; profile_id: string }
  | { t: "ProfileDelete"; id: string; req_id: string; profile: string }
  | { t: "ProfileDeleteOk"; id: string; req_id: string }
  | { t: "WorkflowList"; id: string; req_id: string }
  | { t: "WorkflowListOk"; id: string; req_id: string; workflows: WorkflowSummary[] }
  | { t: "WorkflowGet"; id: string; req_id: string; workflow: string }
  | { t: "WorkflowGetOk"; id: string; req_id: string; workflow: WorkflowSummary; steps: WorkflowStep[] }
  | { t: "WorkflowPut"; id: string; req_id: string; workflow_id?: string | null; draft: WorkflowDraft }
  | { t: "WorkflowPutOk"; id: string; req_id: string; workflow_id: string }
  | { t: "WorkflowDelete"; id: string; req_id: string; workflow: string }
  | { t: "WorkflowDeleteOk"; id: string; req_id: string }
  | { t: "WorkflowRun"; id: string; req_id: string; workflow: string; input?: unknown }
  | { t: "WorkflowRunOk"; id: string; req_id: string; job: string; session: string; pane: string }
  | { t: "TriggerList"; id: string; req_id: string }
  | { t: "TriggerListOk"; id: string; req_id: string; triggers: TriggerRow[] }
  | { t: "TriggerPut"; id: string; req_id: string; trigger_id?: string | null; trigger: TriggerRow }
  | { t: "TriggerPutOk"; id: string; req_id: string; trigger_id: string }
  | { t: "TriggerDelete"; id: string; req_id: string; trigger: string }
  | { t: "TriggerDeleteOk"; id: string; req_id: string }
  | { t: "TriggerRun"; id: string; req_id: string; trigger: string }
  | { t: "TriggerFired"; trigger: string; job: string }
  | { t: "WebhookList"; id: string; req_id: string }
  | { t: "WebhookListOk"; id: string; req_id: string; webhooks: WebhookRow[] }
  | { t: "WebhookPut"; id: string; req_id: string; webhook_id?: string | null; name: string; sources: string[] }
  | { t: "WebhookPutOk"; id: string; req_id: string; webhook_id: string; url: string; secret?: string | null }
  | { t: "WebhookDelete"; id: string; req_id: string; webhook: string }
  | { t: "WebhookDeleteOk"; id: string; req_id: string }
  | { t: "Chunk"; chunk_id: string; i: number; n: number; data: string }
  | { t: "Error"; id?: string; of?: string; req_id?: string; message: string };

let n = 0;
export const nextId = () => `m${++n}`;

export const b64 = (s: string): string => {
  // RN has no Buffer; hand-roll UTF-8 → base64
  const bytes = new TextEncoder().encode(s);
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return globalThis.btoa(bin);
};
