// App-level notification triggers — runs from the always-on relay frame
// handler in App.tsx, so they fire no matter which screen the user is on
// (the point of this app is managing PARALLEL agent sessions; you can't
// be attached to every one of them). Per-pane UI state (busy spinner,
// ask card) stays in Terminal.tsx; this module only decides when a local
// notification should be scheduled.
//
// State is module-level (the app is a single connected machine at a
// time); dedup makes repeated/replayed frames harmless:
// - turn_end: only on a working -> idle edge for a pane we SAW working
//   (an app reconnect can't fabricate a "finished" from a stale idle)
// - every_message: only append frames (reset=false) with seq past the
//   pane's last-seen seq; the first frame for a pane primes the baseline
//   without notifying (history replay protection)
// - questions: once per ask_id, until the answer broadcast arrives
import { notify, toolCallAllowed } from "./notifications";
import type { ChatMsg, Frame } from "./frames";

const busy = new Map<string, boolean>(); // pane -> was working
const lastSeq = new Map<string, number>(); // pane -> last-seen msg seq
const primed = new Set<string>(); // panes whose baseline is established
const notifiedAsks = new Set<string>(); // ask_ids we already notified for

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
        if (f.ask_id && !notifiedAsks.has(f.ask_id)) {
          notifiedAsks.add(f.ask_id);
          void notify(
            "questions",
            name,
            `agent question: ${f.question.slice(0, 110)}`,
            f.pane,
          );
        }
        break;
      case "AgentAskAnswer":
        if (f.ask_id) notifiedAsks.delete(f.ask_id);
        break;
      default:
        break;
    }
  } catch {
    // notifications are best-effort; never break frame handling
  }
}

function onMetaForNotifications(
  f: Extract<Frame, { t: "Meta" }>,
  name: string,
): void {
  if (f.kind !== "agent" || !f.pane) return;
  if (f.status === "working") {
    busy.set(f.pane, true);
  } else if (f.status === "idle" && busy.get(f.pane) === true) {
    busy.set(f.pane, false);
    void notify("turn_end", name, "agent finished its turn", f.pane);
  }
}

function onChatForNotifications(
  f: Extract<Frame, { t: "Chat" }>,
  name: string,
): void {
  const pane = f.pane;
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
          name,
          `agent error: ${text.replace(/\s+/g, " ").slice(0, 120)}`,
          pane,
        );
      } else {
        void notify(
          "every_message",
          name,
          text.replace(/\s+/g, " ").slice(0, 120),
          pane,
        );
      }
    } else if (m.role === "tool" && toolCallAllowed()) {
      void notify("every_message", name, `tool: ${m.tool_name ?? "tool"}`, pane);
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
}
