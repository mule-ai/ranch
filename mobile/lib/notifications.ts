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

export type NotifSettings = {
  turn_end: boolean;
  every_message: boolean;
  ignore_tool_calls: boolean;
  questions: boolean;
  errors: boolean;
};

export const DEFAULT_SETTINGS: NotifSettings = {
  turn_end: true,
  every_message: false,
  ignore_tool_calls: true,
  questions: true,
  errors: true,
};

const KEY = "ranch.settings.v1";
export const CHANNEL = "ranch";

// module-level cache so notify() can check settings synchronously;
// refreshed by loadSettings/saveSettings (both called at app mount)
let cache: NotifSettings = { ...DEFAULT_SETTINGS };
let loaded = false;

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
    for (const k of Object.keys(DEFAULT_SETTINGS) as (keyof NotifSettings)[]) {
      const v = (partial as Record<string, unknown>)[k];
      if (typeof v === "boolean") out[k] = v;
    }
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
  fired: Record<keyof NotifSettings, number>;
  skippedActive: number; // gated off because the app was the active screen
  skippedOff: number; // gated off because the matching setting is off
  errors: number;
};
const stats: NotifStats = {
  fired: { turn_end: 0, every_message: 0, ignore_tool_calls: 0, questions: 0, errors: 0 },
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
    // upsert the "ranch" channel (creates it on first run; Android only)
    await Notifications.setNotificationChannelAsync(CHANNEL, {
      name: "ranch",
      importance: Notifications.AndroidImportance.HIGH,
    });
    moduleError = null;
  } catch (e: any) {
    moduleError = e?.message ?? String(e);
    // notifications are an optional nicety — never crash over them
  }
}

/**
 * Schedule a one-shot local notification if the matching setting is on
 * AND the app is not in the foreground. Returns true when a notification
 * was scheduled, false otherwise (setting off, app active, or any error).
 */
export async function notify(
  kind: keyof NotifSettings,
  title: string,
  body: string,
  threadIdentifier?: string,
): Promise<boolean> {
  try {
    if (!loaded) await loadSettings();
    if (!cache[kind]) {
      stats.skippedOff++;
      logDiag(`[${kind}] skipped: setting off`);
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
    await scheduleOne(title, body, threadIdentifier);
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
): Promise<void> {
  const data: Record<string, unknown> = {};
  if (threadIdentifier) data.threadIdentifier = threadIdentifier;
  await Notifications.scheduleNotificationAsync({
    content: {
      title,
      body,
      data,
    },
    // immediate delivery, on the "ranch" channel (SDK 57: channelId
    // lives on the trigger, not the content)
    trigger: { channelId: CHANNEL },
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
