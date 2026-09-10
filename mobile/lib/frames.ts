// Wire types mirroring crates/ranch-protocol/src/lib.rs (serde tag = "t",
// variant names as-is). Only what the mobile client needs.

export type ForgeSessionInfo = {
  id: string;
  title: string;
  updated: string;
  ended?: string | null;
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
  forge_session?: string;
  agentBusy?: boolean;
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
};

// Binary split tree, mirrors ranch_protocol::Layout
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
  | { t: "HelloOk"; id: string; machine: string; sessions: SessionMeta[] }
  | { t: "Attach"; id: string; client: string; session: string; pane?: string }
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
  | { t: "SessionsCreate"; req_id: string; name?: string; kind?: "shell" | "forge" | "pi"; cwd?: string; forge_session?: string }
  | { t: "ForgeList"; id: string; client: string; req_id: string }
  | { t: "ForgeListOk"; id: string; req_id: string; sessions: ForgeSessionInfo[] }
  | { t: "ChatSend"; id: string; client: string; session: string; pane: string; text: string }
  | { t: "Chat"; id: string; session: string; pane: string; msgs: ChatMsg[]; reset?: boolean }
  | { t: "SessionsAck"; req_id: string; session: string; pane: string }
  | { t: "SessionsRename"; session: string; name: string }
  | { t: "SessionsKill"; session: string }
  | { t: "SessionsSelect"; session: string; pane: string }
  | { t: "PaneSplit"; req_id: string; session: string; pane: string; dir: 0 | 1 }
  | { t: "PaneResize"; session: string; pane: string; dir: 0 | 1; delta: number }
  | { t: "PaneKill"; session: string; pane: string }
  | { t: "Meta"; id: string; session: string; pane?: string; kind: string; status?: string; preview?: string }
  | { t: "Chunk"; chunk_id: string; i: number; n: number; data: string }
  | { t: "Error"; id?: string; of?: string; message: string };

let n = 0;
export const nextId = () => `m${++n}`;

export const b64 = (s: string): string => {
  // RN has no Buffer; hand-roll UTF-8 → base64
  const bytes = new TextEncoder().encode(s);
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return globalThis.btoa(bin);
};
