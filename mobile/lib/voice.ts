// H6.2 voice input (web): browser MediaRecorder -> forge's OpenAI-compatible
// STT proxy (POST /v1/audio/transcriptions -> Parakeet). The forge base URL +
// API key are user settings (settings screen) because this app only talks to
// the daemon through the Supabase relay — there is no direct forge channel
// baked in. On-device recognizer is N/A on web (documented in settings).
import AsyncStorage from "@react-native-async-storage/async-storage";

const KEY = "ranch-voice-config";

export type VoiceConfig = { forgeBase: string; forgeKey: string };

export async function loadVoiceConfig(): Promise<VoiceConfig> {
  try {
    const raw = await AsyncStorage.getItem(KEY);
    if (raw) {
      const p = JSON.parse(raw);
      return {
        forgeBase: typeof p.forgeBase === "string" ? p.forgeBase : "",
        forgeKey: typeof p.forgeKey === "string" ? p.forgeKey : "",
      };
    }
  } catch {
    // fall through to empty config
  }
  return { forgeBase: "", forgeKey: "" };
}

export async function saveVoiceConfig(c: VoiceConfig): Promise<void> {
  await AsyncStorage.setItem(KEY, JSON.stringify(c));
}

/** Web + browser APIs only; the on-device recognizer path has no web
 *  equivalent, so this is the whole voice story on the Expo client. */
export function isVoiceSupported(): boolean {
  try {
    return (
      typeof navigator !== "undefined" &&
      !!navigator.mediaDevices?.getUserMedia &&
      typeof MediaRecorder !== "undefined"
    );
  } catch {
    return false;
  }
}

/**
 * POST the recording to {forgeBase}/v1/audio/transcriptions.
 * Multipart form (the same shape forge's web UI sends): `file` (the
 * recording) + `model` = "parakeet" + `response_format` = "json",
 * `X-API-Key` auth. Success body: `{"text": "..."}` ("text": "" = no
 * speech). Throws on misconfig / non-2xx with a short message.
 */
export async function transcribeViaForge(blob: Blob, cfg: VoiceConfig): Promise<string> {
  const base = cfg.forgeBase.trim().replace(/\/+$/, "");
  if (!base) throw new Error("set the forge base URL in settings");
  const form = new FormData();
  form.append("file", blob, "dictation.webm");
  form.append("model", "parakeet");
  form.append("response_format", "json");
  const resp = await fetch(base + "/v1/audio/transcriptions", {
    method: "POST",
    headers: { "X-API-Key": cfg.forgeKey },
    body: form,
  });
  const body = await resp.text();
  if (!resp.ok) throw new Error(`transcription HTTP ${resp.status}: ${body.slice(0, 160)}`);
  try {
    return (JSON.parse(body).text ?? "").trim();
  } catch {
    return "";
  }
}
