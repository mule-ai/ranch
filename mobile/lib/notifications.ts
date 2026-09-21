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
};

export const DEFAULT_SETTINGS: NotifSettings = {
  turn_end: true,
  every_message: false,
  ignore_tool_calls: true,
  questions: true,
};

const KEY = "ranch.settings.v1";
export const CHANNEL = "ranch";

// module-level cache so notify() can check settings synchronously;
// refreshed by loadSettings/saveSettings (both called at app mount)
let cache: NotifSettings = { ...DEFAULT_SETTINGS };
let loaded = false;

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
    await Notifications.requestPermissionsAsync();
    // upsert the "ranch" channel (creates it on first run; Android only)
    await Notifications.setNotificationChannelAsync(CHANNEL, {
      name: "ranch",
      importance: Notifications.AndroidImportance.HIGH,
    });
  } catch {
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
    if (!cache[kind]) return false;
    // background-only: these notifications exist to get the user's
    // attention when they've put the phone down. While the app is the
    // active screen the user can already see the event in the UI.
    if (AppState.currentState === "active") return false;
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
    return true;
  } catch {
    return false;
  }
}
