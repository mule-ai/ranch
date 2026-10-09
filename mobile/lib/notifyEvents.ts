// App-level notification triggers — H6.1: agent-scoped, card-lane aware.
// Runs from the always-on relay frame handler in App.tsx, so they fire no
// matter which screen the user is on (the point of this app is managing
// PARALLEL agent sessions; you can't be attached to every one of them).
// Per-pane UI state (busy spinner, ask card) stays in Terminal.tsx; this
// module only decides when a local notification should be scheduled.
//
// Policy (kinds, defaults, lane fingerprints, mute/priority) lives in
// ./notify-policy.ts — shared with the native Kotlin client
// (mobile-native .../NotifyPolicy.kt).
//
// Agent identity: AgentAskRequest/Meta/Chat frames carry no agent id, so
// we bind pane/session -> agent from OUR OWN AgentTalk + AgentTalkOk
// frames (export `agentTalkRequested` for the App.tsx send sites; the
// AgentTalkOk reply arrives through onFrameForNotifications). Unresolved
// panes notify under the machine name (graceful degradation).
//
// task_done source: the daemon's activity poller only lists NON-TERMINAL
// forge tasks in ActivityPush, so a previously-seen forge_task item
// leaving the set means its state went terminal (done/failed — not
// distinguishable from the item). Same source as the Kotlin client.
//
// Browser/web parity: the policy decisions + kind mapping below are the
// real shared piece; notification BUTTON actions are NOT wired here
// (browser Notification / expo-notifications local notifications can't
// attach working actions) — buttons ship on the native Kotlin client.
//
// State is module-level (the app is a single connected machine at a
// time); dedup makes repeated/replayed frames harmless:
// - turn_end: only on a working -> idle edge for a pane we SAW working
//   (an app reconnect can't fabricate a "finished" from a stale idle)
// - every_message: only append frames (reset=false) with seq past the
//   pane's last-seen seq; the first frame for a pane primes the baseline
// - questions/cards: once per ask_id, until the answer broadcast arrives
import { notify, toolCallAllowed } from "./notifications";
import { laneForChoices } from "./notify-policy";
import type { ChatMsg, Frame } from "./frames";

const busy = new Map<string, boolean>(); // pane -> was working
const lastSeq = new Map<string, number>(); // pane -> last-seen msg seq
const primed = new Set<string>(); // panes whose baseline is established
const notifiedAsks = new Set<string>(); // ask_ids we already notified for

// H6.1 agent identity
const talkPending = new Map<string, string>(); // AgentTalk req_id -> agent name
const paneAgent = new Map<string, string>(); // pane -> agent name
const sessionAgent = new Map<string, string>(); // session id -> agent name

// H6.1 task_done: forge_task item ids currently in the ActivityPush set
let seenTasks = new Set<string>();
let tasksPrimed = false;

// diagnostics: how many agent-relevant frames reached the app
let agentFramesSeen = 0;
let lastAgentFrameAt: string | null = null;
function noteAgentFrame(): void {
  agentFramesSeen++;
  lastAgentFrameAt = new Date().toTimeString().slice(0, 8);
}
export function getAgentFrameStats(): { seen: number; lastAt: string | null } {
  return { seen: agentFramesSeen, lastAt: lastAgentFrameAt };
}

/** Call from the AgentTalk SEND sites in App.tsx (the outbound frame
 *  never passes through the receive handler) so the AgentTalkOk reply
 *  can be bound to the agent name. */
export function agentTalkRequested(reqId: string, agentName: string): void {
  if (reqId && agentName) talkPending.set(reqId, agentName);
}

function agentFor(pane: string, session?: string): string | undefined {
  return (pane && paneAgent.get(pane)) || (session && sessionAgent.get(session)) || undefined;
}

/**
 * Call from the always-on relay handler for EVERY frame. No-op for
 * anything that doesn't map to a notification. Never throws.
 */
export function onFrameForNotifications(
  f: Frame,
  sessionName: string | undefined,
): void {
  try {
    const name = sessionName || "agent";
    switch (f.t) {
      case "Meta":
        if (f.kind === "agent") noteAgentFrame();
        onMetaForNotifications(f, name);
        break;
      case "Chat":
        noteAgentFrame();
        onChatForNotifications(f, name);
        break;
      case "AgentAskRequest":
        noteAgentFrame();
        onAskForNotifications(f, name);
        break;
      case "AgentAskAnswer":
        if (f.ask_id) notifiedAsks.delete(f.ask_id);
        break;
      case "AgentTalkOk":
        // H6.1: bind the new pane/session to its agent
        const agent = talkPending.get(f.req_id);
        if (agent) {
          talkPending.delete(f.req_id);
          if (f.pane) paneAgent.set(f.pane, agent);
          if (f.session) sessionAgent.set(f.session, agent);
        }
        break;
      case "ActivityPush":
        onActivityPushForNotifications(f);
        break;
      default:
        break;
    }
  } catch {
    // notifications are best-effort; never break frame handling
  }
}

/** H6.1: an AgentAskRequest — classify the card lane (research / memory
 *  / approval / ad-hoc question) and notify under that lane's kind.
 *  The Kotlin client attaches the lane's choice buttons; here the policy
 *  decision + title/body parity is the real piece (documented above). */
function onAskForNotifications(f: Extract<Frame, { t: "AgentAskRequest" }>, name: string): void {
  const askId = f.ask_id;
  if (!askId || notifiedAsks.has(askId)) return;
  notifiedAsks.add(askId);
  const lane = laneForChoices(f.choices ?? []);
  const agent = agentFor(f.pane, f.session);
  const q = (f.question ?? "").replace(/\s+/g, " ").slice(0, 110);
  const body = lane === "questions" ? `agent question: ${q}` : `${lane}: ${q}`;
  void notify(lane, agent ?? name, body, f.pane, agent);
}

/** H6.1 task_done: a forge_task item left the ActivityPush set (its
 *  forge state went terminal). */
function onActivityPushForNotifications(f: Extract<Frame, { t: "ActivityPush" }>): void {
  const now = new Set<string>();
  for (const it of f.items ?? []) {
    if (it.kind !== "forge_task" || !it.agent) continue;
    if (it.id) now.add(it.id);
  }
  if (!tasksPrimed) {
    tasksPrimed = true;
    seenTasks = now;
    return;
  }
  for (const key of seenTasks) {
    if (now.has(key)) continue;
    // item id shape: "forge_task/<agentName>/<taskId>"
    const agent = key.replace(/^forge_task\//, "").split("/").slice(0, -1).join("/") || "agent";
    void notify("task_done", agent, "background task finished", undefined, agent);
  }
  seenTasks = now;
}

function onMetaForNotifications(
  f: Extract<Frame, { t: "Meta" }>,
  name: string,
): void {
  if (f.kind !== "agent" || !f.pane) return;
  const agent = agentFor(f.pane, f.session);
  if (f.status === "working") {
    busy.set(f.pane, true);
  } else if (f.status === "idle" && busy.get(f.pane) === true) {
    busy.set(f.pane, false);
    void notify("turn_end", agent ?? name, "agent finished its turn", f.pane, agent);
  }
}

function onChatForNotifications(
  f: Extract<Frame, { t: "Chat" }>,
  name: string,
): void {
  const pane = f.pane;
  const agent = agentFor(pane, f.session);
  const msgs: ChatMsg[] = f.msgs ?? [];
  if (msgs.length === 0) return;
  const maxSeq = Math.max(...msgs.map((m) => m.seq ?? 0));
  if (f.reset) {
    // history/baseline load — prime, don't notify
    lastSeq.set(pane, maxSeq);
    primed.add(pane);
    return;
  }
  if (!primed.has(pane)) {
    // first frame for this pane and it wasn't a reset — prime it rather
    // than risk notifying on a replayed history batch
    lastSeq.set(pane, maxSeq);
    primed.add(pane);
    return;
  }
  const base = lastSeq.get(pane) ?? -1;
  const fresh = msgs.filter((m) => (m.seq ?? 0) > base);
  if (fresh.length === 0) return;
  lastSeq.set(pane, maxSeq);
  for (const m of fresh) {
    if (m.role === "assistant" && (m.text ?? "").trim() !== "") {
      const text = (m.text ?? "").trim();
      if (text.startsWith("⚠")) {
        // agent error rows (the daemon emits them for failed pi turns,
        // exhausted retries, failed sends) get their own default-on
        // notification — and suppress the follow-up "agent finished its
        // turn" for this pane, because the error IS the turn-end news
        busy.set(pane, false);
        void notify(
          "errors",
          agent ?? name,
          `agent error: ${text.replace(/\s+/g, " ").slice(0, 120)}`,
          pane,
          agent,
        );
      } else {
        void notify(
          "every_message",
          agent ?? name,
          text.replace(/\s+/g, " ").slice(0, 120),
          pane,
          agent,
        );
      }
    } else if (m.role === "tool" && toolCallAllowed()) {
      void notify("every_message", agent ?? name, `tool: ${m.tool_name ?? "tool"}`, pane, agent);
    }
  }
}

/** Drop tracked state when the machine connection drops (reconnect). */
export function resetNotificationState(): void {
  busy.clear();
  lastSeq.clear();
  primed.clear();
  // keep notifiedAsks: an answered question should not re-notify on
  // reconnect, and a still-pending one was already announced.
  seenTasks = new Set();
  tasksPrimed = false;
}
