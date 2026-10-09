// Local notifications for agent events (turn ended, new message, agent
// asked a question). The point is to pull the user BACK into the app,
// so we only schedule when it is in the background — while the user is
// looking at the app the on-screen UI already shows everything and a
// banner would just be noise.
//
// A notification failure (permission denied, scheduling error, …) must
// never crash the app: every public path swallows errors.
import { AppState } from "react-native";
import * as Notifications from "expo-notifications";
import AsyncStorage from "@react-native-async-storage/async-storage";
import {
  KIND_DEFAULTS,
  type NotifyKind,
  type AgentNotifyPrefs,
  AGENT_PREFS_DEFAULT,
  decide,
  channelFor,
} from "./notify-policy";

/** Per-kind toggles + the tool-call behavior switch. */
export type NotifSettings = Record<NotifyKind, boolean> & {
  ignore_tool_calls: boolean;
};

export const DEFAULT_SETTINGS: NotifSettings = {
  ...KIND_DEFAULTS,
  ignore_tool_calls: true,
};

const KEY = "ranch.settings.v1";
const AGENT_KEY = "ranch.agent_notify.v1";
export const CHANNEL = "ranch";

// module-level cache so notify() can check settings synchronously;
// refreshed by loadSettings/saveSettings (both called at app mount)
let cache: NotifSettings = { ...DEFAULT_SETTINGS };
let loaded = false;

// H6.1 per-agent mute/priority (shared policy in notify-policy.ts;
// the Kotlin twin persists the same shape under `agent_notify`).
let agentCache: Record<string, AgentNotifyPrefs> = {};

export async function loadAgentPrefs(): Promise<Record<string, AgentNotifyPrefs>> {
  try {
    const raw = await AsyncStorage.getItem(AGENT_KEY);
    agentCache = raw ? mergeAgentPrefs(JSON.parse(raw)) : {};
  } catch {
    agentCache = {};
  }
  return { ...agentCache };
}

export async function saveAgentPrefs(
  p: Record<string, AgentNotifyPrefs>,
): Promise<void> {
  agentCache = mergeAgentPrefs(p);
  try {
    await AsyncStorage.setItem(AGENT_KEY, JSON.stringify(agentCache));
  } catch {
    // persistence is best-effort; the in-memory cache still applies
  }
}

export function agentPrefsFor(name: string): AgentNotifyPrefs {
  return agentCache[name] ?? AGENT_PREFS_DEFAULT;
}

// Captured once from initNotifications: if the native module is missing
// (e.g. the APK was built before expo-notifications was added) or a
// scheduling call throws, we record the reason so Settings can surface it
// instead of silently no-oping.
let moduleError: string | null = null;
export function getModuleError(): string | null {
  return moduleError;
}

/** True when the native notifications module responded at init time. */
export function moduleAvailable(): boolean {
  return moduleError === null;
}

// merge over defaults; tolerate missing keys, corrupt JSON, wrong types
function merge(partial: unknown): NotifSettings {
  const out: NotifSettings = { ...DEFAULT_SETTINGS };
  if (partial && typeof partial === "object" && !Array.isArray(partial)) {
    for (const k of Object.keys(DEFAULT_SETTINGS)) {
      const v = (partial as Record<string, unknown>)[k];
      if (typeof v === "boolean") out[k as keyof NotifSettings] = v;
    }
  }
  return out;
}

function mergeAgentPrefs(partial: unknown): Record<string, AgentNotifyPrefs> {
  const out: Record<string, AgentNotifyPrefs> = {};
  if (!partial || typeof partial !== "object" || Array.isArray(partial)) return out;
  for (const [k, v] of Object.entries(partial)) {
    if (!v || typeof v !== "object") continue;
    const o = v as Record<string, unknown>;
    out[k] = {
      muted: o.muted === true,
      priority: o.priority === "high" || o.priority === "quiet" ? o.priority : "normal",
    };
  }
  return out;
}

export async function loadSettings(): Promise<NotifSettings> {
  try {
    const raw = await AsyncStorage.getItem(KEY);
    cache = raw ? merge(JSON.parse(raw)) : { ...DEFAULT_SETTINGS };
  } catch {
    cache = { ...DEFAULT_SETTINGS };
  }
  loaded = true;
  return { ...cache };
}

export async function saveSettings(s: NotifSettings): Promise<void> {
  cache = merge(s);
  loaded = true;
  try {
    await AsyncStorage.setItem(KEY, JSON.stringify(cache));
  } catch {
    // persistence is best-effort; the in-memory cache still applies
  }
}

/** Does the current setting allow tool-call notifications? */
export function toolCallAllowed(): boolean {
  return !cache.ignore_tool_calls;
}

// ---------- diagnostics (in-memory, app-session scoped) ----------
// The point: when a user says "it didn't notify", we need to see WHY —
// did agent frames even reach the app? Was the app active (gated off by
// design)? Did a setting block it? Did scheduling throw? This ring buffer
// + counters surface all of that in the Settings screen.
const DIAG_MAX = 40;
const diagLog: string[] = [];
export function logDiag(msg: string): void {
  try {
    const ts = new Date().toTimeString().slice(0, 8);
    diagLog.push(`${ts} ${msg}`);
    if (diagLog.length > DIAG_MAX) diagLog.shift();
  } catch {
    // diagnostics must never throw
  }
}
export function getDiagLog(): string[] {
  return diagLog.slice();
}

export type NotifStats = {
  fired: Record<NotifyKind, number>;
  skippedActive: number; // gated off because the app was the active screen
  skippedOff: number; // gated off because the matching setting off or agent muted
  errors: number;
};
const stats: NotifStats = {
  fired: {
    turn_end: 0,
    every_message: 0,
    errors: 0,
    questions: 0,
    approval: 0,
    research: 0,
    memory: 0,
    task_done: 0,
  },
  skippedActive: 0,
  skippedOff: 0,
  errors: 0,
};
export function getNotifStats(): NotifStats {
  return {
    fired: { ...stats.fired },
    skippedActive: stats.skippedActive,
    skippedOff: stats.skippedOff,
    errors: stats.errors,
  };
}

// safe to call repeatedly (App does it at mount; TerminalScreen too)
export async function initNotifications(): Promise<void> {
  try {
    // foreground behavior: banners/sounds are suppressed by the caller
    // gate below, so this handler only matters for edge cases where a
    // notification slips through while the app is becoming active
    Notifications.setNotificationHandler({
      handleNotification: async () => ({
        shouldShowBanner: false,
        shouldShowList: true,
        shouldPlaySound: true,
        shouldSetBadge: false,
      }),
    });
    // getPermissionsAsync proves the native module is present; a missing
    // module (stale APK) rejects here and we record it for diagnostics.
    const perm = await Notifications.getPermissionsAsync();
    if (!perm.granted) await Notifications.requestPermissionsAsync();
    // upsert the "ranch" channel + the H6.1 priority channels
    // (ranch.quiet / ranch.high, mirroring the Kotlin client)
    await Notifications.setNotificationChannelAsync(CHANNEL, {
      name: "ranch",
      importance: Notifications.AndroidImportance.HIGH,
    });
    await Notifications.setNotificationChannelAsync("ranch.quiet", {
      name: "ranch (quiet)",
      importance: Notifications.AndroidImportance.LOW,
    });
    await Notifications.setNotificationChannelAsync("ranch.high", {
      name: "ranch (high)",
      importance: Notifications.AndroidImportance.HIGH,
    });
    moduleError = null;
  } catch (e: any) {
    moduleError = e?.message ?? String(e);
    // notifications are an optional nicety — never crash over them
  }
}

/**
 * Schedule a one-shot local notification if the matching setting is on,
 * the agent (when resolvable) is not muted, AND the app is not in the
 * foreground. Returns true when a notification was scheduled, false
 * otherwise (setting off, agent muted, app active, or any error).
 */
export async function notify(
  kind: NotifyKind,
  title: string,
  body: string,
  threadIdentifier?: string,
  agent?: string,
): Promise<boolean> {
  try {
    if (!loaded) await loadSettings();
    const agentPrefs = agent ? agentPrefsFor(agent) : undefined;
    if (!decide(Boolean(cache[kind]), agentPrefs)) {
      stats.skippedOff++;
      logDiag(`[${kind}] skipped: setting off or agent muted`);
      return false;
    }
    // background-only: these notifications exist to get the user's
    // attention when they've put the phone down. While the app is the
    // active screen the user can already see the event in the UI.
    if (AppState.currentState === "active") {
      stats.skippedActive++;
      // don't log every active-skip (noisy); it's expected behavior
      return false;
    }
    await scheduleOne(title, body, threadIdentifier, agent);
    stats.fired[kind] = (stats.fired[kind] ?? 0) + 1;
    logDiag(`[${kind}] FIRED -> "${title}"`);
    return true;
  } catch (e: any) {
    stats.errors++;
    logDiag(`[${kind}] ERROR: ${e?.message ?? String(e)}`);
    return false;
  }
}

async function scheduleOne(
  title: string,
  body: string,
  threadIdentifier?: string,
  agent?: string,
): Promise<void> {
  const data: Record<string, unknown> = {};
  if (threadIdentifier) data.threadIdentifier = threadIdentifier;
  if (agent) data.agent = agent;
  // H6.1: per-agent priority picks the delivery channel (the native
  // twin uses ranch.high / ranch.quiet NotificationChannels the same way)
  const channel = channelFor(agent ? agentPrefsFor(agent).priority : undefined);
  await Notifications.scheduleNotificationAsync({
    content: {
      title,
      body,
      data,
    },
    // immediate delivery, on the agent's channel (SDK 57: channelId
    // lives on the trigger, not the content)
    trigger: { channelId: channel },
  });
}

/**
 * Fire a test notification regardless of app state or the per-event
 * toggles — used by the Settings "test" button to verify the native
 * pipeline (module present + permission granted + channel created).
 * Returns a human-readable result so the UI can explain a failure.
 */
export async function testNotification(): Promise<{ ok: boolean; detail: string }> {
  if (moduleError !== null) {
    return { ok: false, detail: `module unavailable: ${moduleError}` };
  }
  try {
    const perm = await Notifications.getPermissionsAsync();
    if (!perm.granted) {
      const req = await Notifications.requestPermissionsAsync();
      if (!req.granted) {
        return { ok: false, detail: "permission not granted — tap 'request' below" };
      }
    }
    await scheduleOne("ranch", "test notification — if you see this, notifications work");
    return { ok: true, detail: "sent — pull down the notification shade" };
  } catch (e: any) {
    moduleError = e?.message ?? String(e);
    return { ok: false, detail: `failed: ${moduleError}` };
  }
}
