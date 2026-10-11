package dev.ranch.android

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.media.MediaRecorder
import android.os.Build
import android.os.Bundle
import android.speech.RecognitionListener
import android.speech.RecognizerIntent
import android.speech.SpeechRecognizer
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.MultipartBody
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONObject
import java.io.File
import java.util.concurrent.TimeUnit

/**
 * H6.2 — voice input for the chat pane. Two modes (Prefs `voice_input`,
 * default [MODE_FORGE]):
 *
 * - [MODE_FORGE] "forge-stt": push-to-talk dictation. The phone records a
 *   16 kHz mono memo ([DictationRecorder]) and POSTs it to forge's
 *   OpenAI-compatible STT proxy `POST /v1/audio/transcriptions` (multipart
 *   `file` + `model=parakeet` + `response_format=json`, `X-API-Key` auth)
 *   which forwards to Parakeet. Preferred: consistent with the voice loop
 *   (H6.3) and handles accents the on-device recognizer mangles.
 * - [MODE_RECOGNIZER] "recognizer": Android's [SpeechRecognizer] with
 *   `EXTRA_PREFER_OFFLINE` — the airplane-mode path. No new deps, no config
 *   (works with no forge base URL/key), no network.
 *
 * Transcripts land in the composer; the user still hits send (v1).
 */
object Voice {
    const val PKEY_MODE = "voice_input"
    const val MODE_FORGE = "forge-stt"
    const val MODE_RECOGNIZER = "recognizer"
    const val PKEY_FORGE_BASE = "forge_base_url"
    const val PKEY_FORGE_KEY = "forge_api_key"

    fun mode(prefs: Prefs): String = prefs.get(PKEY_MODE, MODE_FORGE)

    fun forgeConfigured(prefs: Prefs): Boolean =
        prefs.get(PKEY_FORGE_BASE, "").trim().isNotEmpty() &&
            prefs.get(PKEY_FORGE_KEY, "").trim().isNotEmpty()

    fun hasMicPermission(ctx: Context): Boolean =
        ctx.checkSelfPermission(android.Manifest.permission.RECORD_AUDIO) ==
            PackageManager.PERMISSION_GRANTED
}

/**
 * Cloud voice mode (Herd cloud-voice): instead of the phone holding the
 * lab's forge/mule base URLs + API keys, it signs in to Supabase
 * (Google/password) and routes every voice request through the
 * `ranch-voice` edge function, which verifies the Supabase user JWT and
 * forwards to the endpoints recorded in the user's OWN `voice_gateways`
 * row (migration 0009, RLS-scoped) — the keys never reach the phone.
 * The call's WebSockets are the one exception: Supabase edge cannot
 * carry WS upgrades (platform-rejected), so the call dials the user's
 * public mule base returned by GET /gateway (the 128-bit job id is the
 * capability on those sockets; no key travels).
 */
object CloudVoice {
    const val PKEY_ENABLED = "voice_cloud"
    // local staging prefs for the gateway row (the row itself lives in
    // Supabase's voice_gateways table, owned by the signed-in user).
    const val PKEY_CG_BASE = "cg_base_url"
    const val PKEY_CG_FORGE_KEY = "cg_forge_api_key"
    const val PKEY_CG_MULE_KEY = "cg_mule_api_key"
    const val BASE = Supabase.URL + "/functions/v1/ranch-voice"

    /** Cloud routing is on when enabled in settings AND signed in
     *  (the Supabase access token lives in the same prefs — Auth.kt). */
    fun enabled(prefs: Prefs): Boolean =
        prefs.getBool(PKEY_ENABLED, false) && token(prefs).isNotEmpty()

    fun token(prefs: Prefs): String = prefs.get("access_token", "")

    /** Upsert the user's voice_gateways row via the SECURITY INVOKER RPC
     *  (RLS binds the row to auth.uid()). @return null on success. */
    fun saveGateway(prefs: Prefs): String? {
        val base = prefs.get(PKEY_CG_BASE, "").trim().trimEnd('/')
        val forgeKey = prefs.get(PKEY_CG_FORGE_KEY, "").trim()
        val muleKey = prefs.get(PKEY_CG_MULE_KEY, "").trim()
        val tok = token(prefs)
        if (base.isEmpty() || forgeKey.isEmpty() || muleKey.isEmpty()) {
            return "fill the CG fields first"
        }
        if (tok.isEmpty()) return "sign in to Supabase first"
        val body = JSONObject()
            .put("p_forge_base_url", base)
            .put("p_forge_api_key", forgeKey)
            .put("p_mule_base_url", base)
            .put("p_mule_api_key", muleKey)
            .toString()
        val req = Request.Builder()
            .url(Supabase.URL + "/rest/v1/rpc/upsert_voice_gateway")
            .header("apikey", Supabase.ANON)
            .header("Authorization", "Bearer $tok")
            .header("Content-Type", "application/json")
            .post(body.toRequestBody("application/json".toMediaType()))
            .build()
        return try {
            client.newCall(req).execute().use { resp ->
                val text = resp.body?.string().orEmpty()
                if (resp.code == 204 || resp.isSuccessful) null
                else "HTTP ${resp.code}: ${text.take(160)}"
            }
        } catch (e: Exception) { "${e.message}" }
    }

    private val client = OkHttpClient.Builder()
        .connectTimeout(15, TimeUnit.SECONDS)
        .readTimeout(15, TimeUnit.SECONDS)
        .build()
}

/**
 * 16 kHz mono dictation memo via [MediaRecorder] into an m4a/AAC file
 * (64 kbps AAC-LC; ~0.8 KB/s → ~50 KB for a minute of speech, well under
 * forge's 25 MiB inbound cap). Parakeet sniffs the bytes (wave, then
 * ffmpeg) — ffmpeg decodes m4a, the same path forge's web UI uses for its
 * webm/opus recordings. The phone talks to forge DIRECTLY here (not via
 * the daemon), which is why the base URL + API key live in settings.
 */
class DictationRecorder(private val context: Context) {
    private var rec: MediaRecorder? = null
    private val file = File(context.filesDir, "dictation.m4a")

    /** Create + start recording. Throws on mic/codec failure. */
    fun start() {
        file.delete()
        val r = if (Build.VERSION.SDK_INT >= 31) {
            MediaRecorder(context)
        } else {
            @Suppress("DEPRECATION") MediaRecorder()
        }
        r.setAudioSource(MediaRecorder.AudioSource.MIC)
        r.setOutputFormat(MediaRecorder.OutputFormat.MPEG_4)
        r.setAudioEncoder(MediaRecorder.AudioEncoder.AAC)
        r.setAudioSamplingRate(16000)
        r.setAudioEncodingBitRate(64000)
        r.setOutputFile(file.absolutePath)
        r.prepare()
        r.start()
        rec = r
    }

    /** Stop + release; @return the recorded bytes, or null on failure/empty. */
    fun stop(): ByteArray? {
        val r = rec
        rec = null
        if (r == null) return null
        try { r.stop() } catch (_: Exception) {}
        try { r.release() } catch (_: Exception) {}
        return if (file.length() > 0) file.readBytes() else null
    }

    /** Discard without a usable result (activity teardown, permission loss). */
    fun cancel() {
        stop()
        file.delete()
    }
}

/**
 * Forge STT proxy: `POST {base}/v1/audio/transcriptions`.
 * Multipart form: `file` (the recording) + `model` = "parakeet" +
 * `response_format` = "json" — the same shape forge's web UI sends;
 * `X-API-Key` auth (the proxy also accepts `Authorization: Bearer`).
 * Success body: `{"text": "..."}` (503 when forge has no Parakeet
 * configured, 502 when Parakeet is unreachable — surfaced verbatim).
 */
object ForgeStt {
    private val client = OkHttpClient.Builder()
        .connectTimeout(5, TimeUnit.SECONDS)
        .readTimeout(30, TimeUnit.SECONDS)
        .build()

    /** @return transcript ("" = no speech detected), or failure with the error.
     *  [bearer] switches auth to `Authorization: Bearer` (cloud mode via the
     *  ranch-voice edge function; leave null + set [apiKey] for direct use). */
    fun transcribe(
        base: String,
        apiKey: String?,
        audio: ByteArray,
        filename: String,
        bearer: String? = null,
    ): Result<String> {
        val url = base.trim().trimEnd('/') + "/v1/audio/transcriptions"
        val form = MultipartBody.Builder().setType(MultipartBody.FORM)
            .addFormDataPart("file", filename,
                audio.toRequestBody("audio/mp4".toMediaType()))
            .addFormDataPart("model", "parakeet")
            .addFormDataPart("response_format", "json")
            .build()
        val b = Request.Builder().url(url).post(form)
        if (bearer != null) b.header("Authorization", "Bearer $bearer")
        else b.header("X-API-Key", apiKey ?: "")
        val req = b.build()
        return try {
            client.newCall(req).execute().use { resp ->
                val body = resp.body?.string() ?: ""
                if (!resp.isSuccessful) {
                    Result.failure(Exception("HTTP ${resp.code}: ${body.take(200)}"))
                } else {
                    Result.success(JSONObject(body).optString("text", "").trim())
                }
            }
        } catch (e: Exception) {
            Result.failure(e)
        }
    }
}

/**
 * On-device speech recognition (airplane-mode path). System [SpeechRecognizer]
 * service with `EXTRA_PREFER_OFFLINE` — no permission of its own (the system
 * service holds the mic), no config. Results arrive async via [onResult];
 * no-match / no-service land in [onError]. [cancel] + [destroy] are safe to
 * call from teardown.
 */
class OnDeviceRecognizer(
    activity: Activity,
    private val onResult: (String) -> Unit,
    private val onError: (String) -> Unit,
) {
    private val sr = SpeechRecognizer.createSpeechRecognizer(activity)
    private var done = false

    init {
        sr.setRecognitionListener(object : RecognitionListener {
            override fun onReadyForSpeech(params: Bundle?) {}
            override fun onBeginningOfSpeech() {}
            override fun onRmsChanged(rmsdB: Float) {}
            override fun onBufferReceived(buffer: ByteArray?) {}
            override fun onPartialResults(results: Bundle?) {}
            override fun onEndOfSpeech() {}
            override fun onError(error: Int) {
                if (!done) { done = true; onError(errorMsg(error)) }
            }
            override fun onResults(results: Bundle?) {
                if (done) return
                done = true
                val first = results?.getStringArrayList(RecognizerIntent.EXTRA_RESULTS)?.firstOrNull()
                onResult(first ?: "")
            }
            override fun onEvent(eventType: Int, params: Bundle?) {}
        })
    }

    /** Launch a recognition session. Throws if no recognition service exists. */
    fun start() {
        val i = Intent(RecognizerIntent.ACTION_RECOGNIZE_SPEECH).apply {
            putExtra(RecognizerIntent.EXTRA_LANGUAGE_MODEL, RecognizerIntent.LANGUAGE_MODEL_FREE_FORM)
            putExtra(RecognizerIntent.EXTRA_PARTIAL_RESULTS, false)
            putExtra(RecognizerIntent.EXTRA_PREFER_OFFLINE, true)
        }
        sr.startListening(i)
    }

    fun cancel() {
        try { sr.cancel() } catch (_: Exception) {}
    }

    fun destroy() {
        try { sr.destroy() } catch (_: Exception) {}
    }

    private fun errorMsg(e: Int): String = when (e) {
        SpeechRecognizer.ERROR_NO_MATCH -> "no speech detected"
        SpeechRecognizer.ERROR_NETWORK -> "speech service unreachable"
        SpeechRecognizer.ERROR_AUDIO -> "audio capture failed"
        else -> "speech error (code $e)"
    }
}
