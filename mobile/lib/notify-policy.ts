// H6.1 — SHARED NOTIFICATION POLICY (the source of truth).
//
// This module is the policy for BOTH phone clients:
//  - the Expo app consumes it directly (`notifications.ts`,
//    `notifyEvents.ts`)
//  - the native Kotlin client mirrors it in
//    `mobile-native/app/src/main/java/dev/ranch/android/NotifyPolicy.kt`
//    (no cross-runtime code sharing is realistic — JVM vs JS — so the
//    Kotlin file is a hand-maintained mirror; keep the two in sync).
//
// Kinds and defaults:
//   turn_end     true   pane working -> idle edge (re-tagged with the
//                       agent name when the pane resolves to an agent)
//   errors       true   `⚠` assistant chat rows
//   questions    true   ad-hoc AgentAsk with no recognizable card lane
//   approval     true   policy_ask card ("Allow this action" / "Deny")
//   research     true   research_report card ("Use" / "Discard" / "Ask more")
//   memory       true   memory_review card ("Keep" / "Forget")
//   task_done    true   a forge_task activity item left the ActivityPush
//                       set (its forge state went terminal — the poller
//                       only lists pending/running tasks; a feed outage
//                       is indistinguishable and also fires this)
//   every_message false (only when turn_end is off)
//
// Mute/priority: `muted` suppresses notifications ONLY — the agent's
// items still appear in the Activity View. `quiet` = low-importance,
// silent delivery; `high` = high-importance delivery; `normal` = default.
//
// Browser/web note: the browser Notification API (and expo-notifications
// local notifications) cannot attach working button ACTIONS, so on this
// app the policy decisions and kind mapping below are the shared,
// real piece; action BUTTONS ship on the native Kotlin client (its
// buttons route AgentAskAnswer through the foreground service's Realtime
// socket). The labels in `actionLabels()` document what those buttons
// would say here.

export type NotifyKind =
  | "turn_end"
  | "every_message"
  | "errors"
  | "questions"
  | "approval"
  | "research"
  | "memory"
  | "task_done";

export const KIND_DEFAULTS: Record<NotifyKind, boolean> = {
  turn_end: true,
  every_message: false,
  errors: true,
  questions: true,
  approval: true,
  research: true,
  memory: true,
  task_done: true,
};

/** Kinds whose notification would carry actionable buttons. */
export const ACTIONABLE_KINDS: ReadonlySet<NotifyKind> = new Set([
  "questions",
  "approval",
  "research",
  "memory",
]);

// ---- per-agent settings (Kotlin twin: NotifyPolicy.AgentPrefs) ----
export type AgentPriority = "high" | "normal" | "quiet";
export interface AgentNotifyPrefs {
  muted: boolean;
  priority: AgentPriority;
}
export const AGENT_PREFS_DEFAULT: AgentNotifyPrefs = {
  muted: false,
  priority: "normal",
};

/** kind enabled (user toggle) AND agent not muted → notify.
 *  Mute is notification-only; the Activity View is unaffected. */
export function decide(
  kindEnabled: boolean,
  agentPrefs: AgentNotifyPrefs | undefined,
): boolean {
  return kindEnabled && agentPrefs?.muted !== true;
}

/** Which channel/importance a notification uses for a priority. */
export function channelFor(priority: AgentPriority | undefined): string {
  switch (priority) {
    case "high":
      return "ranch.high";
    case "quiet":
      return "ranch.quiet";
    default:
      return "ranch";
  }
}

// ---- ask-lane classification (Kotlin twin: laneForChoices) ----
// The AgentAskRequest frame carries no lane field; the daemon's card
// lanes are fingerprinted by their exact choice sets (ranch forge.rs):
//   memory_review    -> ["Keep", "Forget"]
//   research_report  -> ["Use", "Discard", "Ask more"]
//   policy_ask       -> ["Allow this action", "Deny"]
//   anything else    -> ad-hoc "questions" (still actionable if it has
//                       choices — buttons are harmless and useful)
export function laneForChoices(
  choices: string[],
): "research" | "memory" | "approval" | "questions" {
  const c = choices.map((s) => s.trim().toLowerCase());
  if (c.length === 3 && c[0] === "use" && c[1] === "discard" && c[2] === "ask more")
    return "research";
  if (c.length === 2 && c[0] === "keep" && c[1] === "forget") return "memory";
  if (c.length === 2 && c[0].startsWith("allow")) return "approval";
  return "questions";
}

/** Which choice indices become notification buttons (max 2). */
export function actionIndices(choices: string[]): number[] {
  return Array.from({ length: Math.min(2, choices.length) }, (_, i) => i);
}

/** Button labels: the first two choice texts, clamped to 24 chars. */
export function actionLabels(choices: string[]): string[] {
  return actionIndices(choices).map((i) => labelForChoice(choices[i]));
}

export function labelForChoice(choice: string): string {
  const t = (choice ?? "").trim();
  if (!t) return "OK";
  return t.length > 24 ? `${t.slice(0, 21)}…` : t;
}

// ---- avatar cache naming (Kotlin twin: avatarFileName) ----
// The native client downloads registry avatars to
// `cacheDir/avatars/<first 8 bytes of sha256(url) in hex>`.
export const AVATAR_CACHE_DIR = "avatars";
