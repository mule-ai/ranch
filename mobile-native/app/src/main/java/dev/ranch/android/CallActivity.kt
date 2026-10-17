package dev.ranch.android

import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.AudioTrack
import android.media.MediaRecorder
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import okio.ByteString
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import org.json.JSONObject
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.math.sqrt

/**
 * H6.3/H6.4 — voice loop v1 + full-duplex barge-in v2 (the "call" screen,
 * the 📞 button on an agent row).
 *
 * The phone talks to MULE directly — the same transport decision as
 * `ranch call` (PROTOCOL.md §8.7: no ranch frames, the daemon is out of
 * the media path):
 *
 *  1. REST: find this agent's `voice_call` workflow (`GET /api/v1/workflows`
 *     + each workflow's steps, matching the step whose config.agent_id is
 *     the agent — the CLI's `pick_voice_workflow` does the same), then
 *     `POST /api/v1/jobs` to start it.
 *  2. IN  `ws {mule}/voice/{job}/in` — 30 ms binary frames of 16 kHz mono
 *     s16le from [AudioRecord]; text `{"hangup":true}` ends the call.
 *     If the mic permission is denied, a typed-text fallback row appears
 *     instead (`{"text": …}` per utterance — no STT hop).
 *  3. OUT `ws {mule}/voice/{job}/out` — binary TTS PCM chunks →
 *     [AudioTrack] (rate from the step's `tts_sample_rate`, default 24
 *     kHz; the out socket carries no rate metadata, both surfaces read
 *     the config); text `{"assistant": …}` appends to the transcript;
 *     text `{"notice": …}` (degraded hop: stt/tts/agent failure) shows a
 *     ⚠ transcript line — never unexplained silence; `{"done":true}` =
 *     the call ended.
 *
 * H6.4 v2 — the two full-duplex pieces this screen owns:
 *
 *  - **Flush control.** On a barge-in (you start talking while the
 *    agent's TTS is playing), mule drains its out-queue and sends
 *    `{"flush":true}` on the out socket. The [AudioTrack] still holds
 *    queued PCM beyond mule's knowledge, so this screen stops + flushes
 *    + restarts the track immediately (stop-to-silence on the phone is
 *    the client-side half of mule's <150 ms barge-in budget; mule-side
 *    stop-to-drain is asserted in its unit tests).
 *  - **Echo guard.** While the local mic is hot (the user is speaking),
 *    the out-channel gain is zeroed (`AudioTrack.setVolume(0f)`) so the
 *    phone speaker can't ring the user's own voice back into the mic —
 *    the speakerphone feedback loop. Hysteresis: a few consecutive hot
 *    frames mute, the trailing silence (a few cold frames) restores.
 *    The RMS thresholds sit just under mule's barge-in floor, so the
 *    guard is hot exactly when mule would trigger a barge-in.
 *
 * Hang-up: the 📞-hangup button or the back button sends the hang-up
 * frame and closes both sockets; mule treats a closed in socket as a
 * hang-up too (job COMPLETED, "call dropped" only on mule-side crash).
 */
class CallActivity : Activity() {

    companion object {
        const val PKEY_MULE_BASE = "mule_base_url"
        const val PKEY_MULE_KEY = "mule_api_key"
        const val EXTRA_AGENT_ID = "agent_id"
        const val EXTRA_AGENT_NAME = "agent_name"
        private const val MIC_RATE = 16_000
        private const val FRAME_BYTES = 960 // 30 ms @ 16 kHz s16le mono
        private const val DEFAULT_TTS_RATE = 24_000
        private const val REQ_MIC = 7
        // Echo guard (H6.4): mute the TTS while the local mic is hot.
        // Mule's barge-in floor is RMS 600 @ 16 kHz; the guard sits just
        // under it so the out-channel is already silent when mule
        // triggers, and hysteresis (hot/cold gap + streaks) stops the
        // gain flickering on breathy speech.
        private const val MIC_HOT_RMS = 400.0
        private const val MIC_COLD_RMS = 200.0
        private const val HOT_STREAK_MUTE = 3 // 3 frames ≈ 90 ms of speech
        private const val COLD_STREAK_UNMUTE = 8 // 8 frames ≈ 240 ms trailing silence
    }

    private val app get() = application as App
    private val handler = Handler(Looper.getMainLooper())
    private val exec = java.util.concurrent.Executors.newSingleThreadExecutor()
    private val http = OkHttpClient.Builder()
        .connectTimeout(6, TimeUnit.SECONDS)
        .readTimeout(20, TimeUnit.SECONDS)
        .build()

    private var agentId = ""
    private var agentName = ""

    private lateinit var status: TextView
    private lateinit var transcript: TextView
    private lateinit var hangupBtn: Button
    private var typeRow: LinearLayout? = null
    private var typeEdit: EditText? = null

    private var inWs: WebSocket? = null
    private var outWs: WebSocket? = null
    private var recorder: AudioRecord? = null
    private var player: AudioTrack? = null
    private val recRunning = AtomicBoolean(false)
    @Volatile private var ended = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        agentId = intent.getStringExtra(EXTRA_AGENT_ID).orEmpty()
        agentName = intent.getStringExtra(EXTRA_AGENT_NAME).orEmpty()
        if (agentId.isEmpty()) { finish(); return }

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(0xFF101418.toInt())
            setPadding(dp(12), dp(12), dp(12), dp(12))
        }
        val bar = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        bar.addView(Button(this).apply {
            text = "←"
            setOnClickListener { hangUp("leaving") }
        })
        bar.addView(TextView(this).apply {
            text = "Call · ${agentName.ifEmpty { agentId.take(8) }}"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 17f)
            setTextColor(0xFFE5E5E5.toInt())
            gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(8), 0, 0, 0)
        }, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.MATCH_PARENT, 1f))
        root.addView(bar)

        status = TextView(this).apply {
            text = "starting…"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setTextColor(0xFF9aa0a6.toInt())
            setPadding(0, dp(4), 0, dp(8))
        }
        root.addView(status)

        transcript = TextView(this).apply {
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setTextColor(0xFFd1d5db.toInt())
        }
        root.addView(ScrollView(this).apply {
            addView(transcript)
            layoutParams = LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f)
        })

        // Typed-text fallback row (visible only when the mic is
        // unavailable): one utterance per line, "bye" hangs up.
        val type = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        val et = EditText(this).apply {
            hint = "type a line (or 'bye')"
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(0xFFC9CDD3.toInt())
            setHintTextColor(0xFF5C636B.toInt())
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_SENTENCES
            setPadding(0, dp(8), 0, dp(8))
        }
        type.addView(et, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        type.addView(Button(this).apply {
            text = "send"
            setOnClickListener {
                val s = et.text.toString().trim()
                if (s.isEmpty()) return@setOnClickListener
                et.text.clear()
                if (s.equals("bye", true)) hangUp("bye") else sendTyped(s)
            }
        })
        type.visibility = View.GONE
        typeRow = type
        typeEdit = et
        root.addView(type)

        hangupBtn = Button(this).apply {
            text = "Hang up"
            setPadding(0, dp(8), 0, dp(8))
            setOnClickListener { hangUp("hung up") }
        }
        root.addView(hangupBtn)

        setContentView(root)
        applyEdgeToEdgeInsets(findViewById(android.R.id.content))

        val cloud = CloudVoice.enabled(app.prefs)
        val base = if (cloud) CloudVoice.BASE
        else app.prefs.get(PKEY_MULE_BASE, "").trim().trimEnd('/')
        val key = if (cloud) CloudVoice.token(app.prefs)
        else app.prefs.get(PKEY_MULE_KEY, "").trim()
        if (cloud && key.isEmpty()) {
            status.text = "cloud voice needs a Supabase sign-in (Google) — sign in on the main screen, or use direct mode"
            status.setTextColor(0xFFF59E0B.toInt())
            root.addView(Button(this).apply {
                text = "Open settings"
                setOnClickListener {
                    startActivity(Intent(this@CallActivity, SettingsActivity::class.java))
                }
            })
            return
        }
        if (base.isEmpty() || key.isEmpty()) {
            status.text = if (cloud) "cloud voice not enabled" else "mule is not configured"
            status.setTextColor(0xFFF59E0B.toInt())
            root.addView(Button(this).apply {
                text = "Open settings"
                setOnClickListener {
                    startActivity(Intent(this@CallActivity, SettingsActivity::class.java))
                }
            })
            return
        }
        startCall(base, key, cloud)
    }

    // ------------------------------------------------------------------
    // call start: find the voice_call workflow, start the job, dial WS
    // ------------------------------------------------------------------

    private fun startCall(base: String, key: String, cloud: Boolean) {
        exec.execute {
            val wfResult = findVoiceWorkflow(base, key, agentId)
            val found = wfResult.getOrNull()
            if (found == null) {
                val err = wfResult.exceptionOrNull()
                if (err != null) {
                    fail((err.message ?: "workflow lookup failed") +
                        if (cloud) "\n\n(cloud: this Supabase account must have a saved cloud gateway — Settings → Save cloud gateway; and the tunnel must be up on the lab)" else "")
                    return@execute
                }
                status2("no voice_call workflow for this agent — seed one (mule examples/voice-loop/seed.sh)")
                ended = true
                return@execute
            }
            val (wfId, ttsRate) = found
            val jobResult = startJob(base, key, wfId)
            val jobId = jobResult.getOrNull()
            if (jobId == null) {
                fail(jobResult.exceptionOrNull()?.message ?: "no job id from mule")
                return@execute
            }
            status2("dialing… (job ${jobId.take(8)})")
            // cloud: the WebSockets can't cross the Supabase edge (the
            // platform rejects WS upgrades), so the call dials the user's
            // PUBLIC mule base that the /gateway endpoint resolves from
            // their voice_gateways row. Direct mode dials the base itself.
            var wsBase = base
            if (cloud) {
                wsBase = (muleGet(base, key, "/gateway").getOrNull() as? JSONObject)
                    ?.optString("mule_base_url")?.trim()?.trimEnd('/') ?: ""
                if (wsBase.isEmpty()) {
                    ended = true
                    fail("cloud: /gateway returned no mule base — set your public tunnel URL in the voice gateway settings")
                    return@execute
                }
            }
            try {
                connectOut(wsBase, jobId, ttsRate)
                connectIn(wsBase, jobId)
            } catch (e: Exception) {
                fail("ws connect: ${e.message}")
                return@execute
            }
            if (Voice.hasMicPermission(this)) {
                startRecorder()
                status2("listening — speak; 'Hang up' ends the call")
            } else {
                // requestPermissions must run on the main thread; the
                // answer lands in onRequestPermissionsResult (grant →
                // startRecorder, denial → typed text).
                handler.post { requestPermissions(arrayOf(android.Manifest.permission.RECORD_AUDIO), REQ_MIC) }
                status2("microphone permission requested…")
            }
        }
    }

    /**
     * The agent's voice_call workflow: (workflow id, TTS sample rate).
     * Same walk as the CLI's `pick_voice_workflow`.
     */
    private fun findVoiceWorkflow(
        base: String,
        key: String,
        agentId: String,
    ): Result<Pair<String, Int>?> {
        val wfRes = muleGet(base, key, "/api/v1/workflows")
        val wfs = wfRes.getOrElse { return resFail(it.message ?: "list workflows") }
        // mule's list endpoints are inconsistent: /api/v1/workflows is a BARE
        // array, /api/v1/wakes is {wakes: [...]}, job POST is {data: {...}} —
        // accept every shape we've seen.
        val arr = when (wfs) {
            is JSONObject -> wfs.optJSONArray("workflows") ?: wfs.optJSONArray("data")
            is org.json.JSONArray -> wfs
            else -> null
        }
        if (arr == null) return resFail("list workflows: unexpected shape: ${wfs}")
        var firstErr: String? = null
        for (i in 0 until arr.length()) {
            val wf = arr.getJSONObject(i)
            val wfId = wf.optString("id")
            if (wfId.isEmpty()) continue
            val steps = muleGet(base, key, "/api/v1/workflows/$wfId/steps")
                .getOrElse { firstErr = firstErr ?: (it.message ?: "steps"); continue }
            val sArr = if (steps is JSONObject) steps.optJSONArray("steps") else null
            val list = sArr ?: (steps as? org.json.JSONArray) ?: continue
            for (j in 0 until list.length()) {
                val st = list.getJSONObject(j)
                if (st.optString("type") != "voice_call") continue
                val cfg = st.optJSONObject("config") ?: continue
                if (cfg.optString("agent_id") != agentId) continue
                val rate = cfg.optInt("tts_sample_rate", DEFAULT_TTS_RATE).takeIf {
                    it in 1..2_000_000
                } ?: DEFAULT_TTS_RATE
                return Result.success(wfId to rate)
            }
        }
        if (firstErr != null && arr.length() == 0) return resFail(firstErr)
        return Result.success(null as Pair<String, Int>?)
    }

    private fun startJob(base: String, key: String, workflowId: String): Result<String> {
        val body = JSONObject().put("workflow_id", workflowId).toString()
        val req = Request.Builder()
            .url(base + "/api/v1/jobs")
            .header("Authorization", "Bearer $key")
            .post(body.toRequestBody("application/json".toMediaType()))
            .build()
        return try {
            http.newCall(req).execute().use { resp ->
                val text = resp.body?.string().orEmpty()
                if (!resp.isSuccessful) resFail("HTTP ${resp.code}: ${text.take(200)}")
                // mule wraps the created job in {"data": {...}} (the GET by id
                // is a bare object — same API, two shapes).
                else {
                    val j = JSONObject(text)
                    val id = j.optJSONObject("data")?.optString("id")?.takeIf { it.isNotEmpty() }
                        ?: j.optString("id")
                    if (id.isEmpty()) resFail("job id not in response: ${text.take(200)}")
                    else Result.success(id)
                }
            }
        } catch (e: Exception) {
            resFail(e.message ?: "request failed")
        }
    }

    /**
     * GET with the real error surfaced: the previous version swallowed
     * every failure as `null`, so a 401/404/DNS failure showed up as the
     * useless "list workflows".
     */
    private fun muleGet(base: String, key: String, path: String): Result<Any?> = try {
        val req = Request.Builder()
            .url(base + path)
            .header("Authorization", "Bearer $key")
            .get()
            .build()
        http.newCall(req).execute().use { resp ->
            val text = resp.body?.string().orEmpty()
            if (!resp.isSuccessful) {
                return@use Result.failure<Any?>(
                    Exception("HTTP ${resp.code} on $path: ${text.take(200)}")
                )
            }
            try {
                when {
                    text.startsWith("[") -> Result.success<Any?>(org.json.JSONArray(text))
                    text.startsWith("{") -> Result.success<Any?>(JSONObject(text))
                    else -> Result.failure<Any?>(Exception("$path: unexpected body: ${text.take(120)}"))
                }
            } catch (e: Exception) {
                Result.failure<Any?>(Exception("$path: bad JSON: ${text.take(120)}"))
            }
        }
    } catch (e: Exception) {
        Result.failure<Any?>(Exception("$path: ${e.message}"))
    }

    // ------------------------------------------------------------------
    // the two voice sockets
    // ------------------------------------------------------------------

    private fun connectIn(base: String, jobId: String) {
        val url = wsUrl(base) + "/voice/$jobId/in"
        inWs = http.newWebSocket(request(url), object : WebSocketListener() {
            override fun onFailure(ws: WebSocket, t: Throwable, response: Response?) {
                if (!ended) onCallDropped("in socket: ${t.message}")
            }

            override fun onClosed(ws: WebSocket, code: Int, reason: String) {
                onCallDropped("call ended")
            }
        })
    }

    private fun connectOut(base: String, jobId: String, ttsRate: Int) {
        val url = wsUrl(base) + "/voice/$jobId/out"
        outWs = http.newWebSocket(request(url), object : WebSocketListener() {
            override fun onOpen(ws: WebSocket, response: Response) {
                player = try {
                    val min = AudioTrack.getMinBufferSize(
                        ttsRate, AudioFormat.CHANNEL_OUT_MONO, AudioFormat.ENCODING_PCM_16BIT)
                    AudioTrack(
                        AudioAttributes.Builder()
                            .setUsage(AudioAttributes.USAGE_MEDIA)
                            .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
                            .build(),
                        AudioFormat.Builder()
                            .setSampleRate(ttsRate)
                            .setChannelMask(AudioFormat.CHANNEL_OUT_MONO)
                            .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                            .build(),
                        maxOf(min, 4096),
                        AudioTrack.MODE_STREAM,
                        0,
                    ).also { it.play() }
                } catch (e: Exception) {
                    status2("speaker unavailable: ${e.message} (transcript only)")
                    null
                }
            }

            override fun onMessage(ws: WebSocket, bytes: okio.ByteString) {
                val p = player ?: return
                val arr = bytes.toByteArray()
                try {
                    p.write(arr, 0, arr.size)
                } catch (_: Exception) {
                    // player died: drop the rest (documented v1 behavior)
                }
            }

            override fun onMessage(ws: WebSocket, text: String) {
                val o = try { JSONObject(text) } catch (e: Exception) { return }
                if (o.optBoolean("done", false)) {
                    onCallEnded("call ended")
                    return
                }
                if (o.optBoolean("flush", false)) {
                    // H6.4 barge-in: mule drained its out-queue; drop the
                    // phone's playback buffer too (the AudioTrack holds
                    // queued PCM beyond mule's knowledge). stop() halts
                    // the stream, flush() clears the queued PCM, play()
                    // restarts so the next TTS chunk flows.
                    val p = player
                    if (p != null) try {
                        p.stop()
                        p.flush()
                        p.play()
                    } catch (_: Exception) {
                        // track already dead — new writes will fail and
                        // be dropped, the transcript keeps working
                    }
                    return
                }
                val n = o.optString("notice", "")
                if (n.isNotEmpty()) {
                    // mule degraded hop (stt/tts/agent failure): show WHY
                    // instead of leaving the caller in silence.
                    appendTranscript("⚠ $n")
                    return
                }
                val a = o.optString("assistant", "")
                if (a.isNotEmpty()) appendTranscript("agent: $a")
            }

            override fun onFailure(ws: WebSocket, t: Throwable, response: Response?) {
                if (!ended) onCallDropped("out socket: ${t.message}")
            }

            override fun onClosed(ws: WebSocket, code: Int, reason: String) {
                onCallDropped("call ended")
            }
        })
    }

    private fun request(url: String): Request = Request.Builder().url(url).build()

    private fun wsUrl(base: String): String =
        when {
            base.startsWith("https://") -> "wss://" + base.removePrefix("https://")
            else -> "ws://" + base.removePrefix("http://")
        }

    // ------------------------------------------------------------------
    // mic capture
    // ------------------------------------------------------------------

    private fun startRecorder() {
        exec.execute {
            val min = AudioRecord.getMinBufferSize(
                MIC_RATE, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT)
            val rec = try {
                AudioRecord(
                    MediaRecorder.AudioSource.MIC,
                    MIC_RATE,
                    AudioFormat.CHANNEL_IN_MONO,
                    AudioFormat.ENCODING_PCM_16BIT,
                    maxOf(min, FRAME_BYTES * 4),
                )
            } catch (e: Exception) {
                typedMode("mic capture failed (${e.message}) — typed text mode")
                return@execute
            }
            if (rec.state != AudioRecord.STATE_INITIALIZED) {
                typedMode("mic capture failed — typed text mode")
                rec.release()
                return@execute
            }
            recorder = rec
            recRunning.set(true)
            rec.startRecording()
            val buf = ByteArray(FRAME_BYTES)
            val ws = inWs
            var hotStreak = 0
            var coldStreak = 0
            var muted = false
            while (recRunning.get()) {
                val n = rec.read(buf, 0, buf.size)
                if (n <= 0) continue
                if (n < buf.size) System.arraycopy(buf, 0, buf, 0, n)
                ws?.send(ByteString.of(*buf.copyOf(n))) ?: run {
                    onCallDropped("in socket gone")
                    break
                }
                // Echo guard (H6.4): zero the out-channel gain while the
                // user is speaking so the speaker can't feed their own
                // voice back into the mic. Capture-thread only; the
                // AudioTrack volume is safe to set from any thread.
                val rms = frameRms(buf, n)
                when {
                    rms >= MIC_HOT_RMS -> { hotStreak++; coldStreak = 0 }
                    rms < MIC_COLD_RMS -> { coldStreak++; hotStreak = 0 }
                    else -> { hotStreak = 0; coldStreak = 0 }
                }
                val p = player
                if (p != null) {
                    if (!muted && hotStreak >= HOT_STREAK_MUTE) {
                        muted = true
                        try { p.setVolume(0f) } catch (_: Exception) {}
                    } else if (muted && coldStreak >= COLD_STREAK_UNMUTE) {
                        muted = false
                        try { p.setVolume(1f) } catch (_: Exception) {}
                    }
                }
            }
            rec.stop()
            rec.release()
            recorder = null
        }
    }

    private fun stopRecorder() {
        recRunning.set(false)
    }

    /** RMS of an s16le PCM frame (the same 0..32767 scale mule's VAD uses). */
    private fun frameRms(buf: ByteArray, n: Int): Float {
        var sumSq = 0.0
        var i = 0
        while (i + 1 < n) {
            val v = (buf[i].toInt() and 0xFF) or (buf[i + 1].toInt() shl 8)
            sumSq += v.toDouble() * v
            i += 2
        }
        val c = n / 2
        return if (c == 0) 0f else sqrt(sumSq / c).toInt().toFloat()
    }

    private fun typedMode(reason: String) {
        stopRecorder()
        handler.post {
            typeRow?.visibility = View.VISIBLE
            status2(reason)
        }
    }

    private fun sendTyped(s: String) {
        inWs?.send(JSONObject().put("text", s).toString())
        appendTranscript("you: $s")
    }

    // ------------------------------------------------------------------
    // teardown + status
    // ------------------------------------------------------------------

    private fun hangUp(why: String) {
        if (ended) { finish(); return }
        ended = true
        stopRecorder()
        try { inWs?.send("""{"hangup":true}""") } catch (_: Exception) {}
        try { inWs?.close(1000, null) } catch (_: Exception) {}
        try { outWs?.close(1000, null) } catch (_: Exception) {}
        handler.post {
            appendTranscript("— $why —")
            status2("call ended")
            hangupBtn.isEnabled = false
            finish()
        }
    }

    private fun onCallDropped(why: String) {
        if (ended) return
        ended = true
        stopRecorder()
        try { player?.stop() } catch (_: Exception) {}
        try { inWs?.close(1000, null) } catch (_: Exception) {}
        try { outWs?.close(1000, null) } catch (_: Exception) {}
        handler.post {
            appendTranscript("— call dropped ($why) —")
            status2("call dropped")
            status.setTextColor(0xFFF59E0B.toInt())
            hangupBtn.isEnabled = false
        }
    }

    private fun onCallEnded(why: String) {
        if (ended) return
        ended = true
        stopRecorder()
        try { player?.stop() } catch (_: Exception) {}
        try { inWs?.close(1000, null) } catch (_: Exception) {}
        try { outWs?.close(1000, null) } catch (_: Exception) {}
        handler.post {
            appendTranscript("— $why —")
            status2(why)
            hangupBtn.isEnabled = false
        }
    }

    private fun appendTranscript(line: String) {
        handler.post {
            transcript.append(line + System.lineSeparator())
            // keep the view bounded (the transcript is a status line,
            // not a store)
            val t = transcript.text
            if (t.length > 20_000) transcript.setText(t.subSequence(t.length - 10_000, t.length))
        }
    }

    private fun status2(msg: String) = handler.post { status.text = msg }

    private fun fail(msg: String) {
        ended = true
        handler.post {
            status.text = msg
            status.setTextColor(0xFFF59E0B.toInt())
        }
    }

    override fun onDestroy() {
        if (!ended) {
            // Activity going away without an explicit hang-up: close the
            // sockets — mule treats the in-socket close as a hang-up.
            ended = true
            stopRecorder()
            try { player?.stop() } catch (_: Exception) {}
            try { inWs?.close(1000, null) } catch (_: Exception) {}
            try { outWs?.close(1000, null) } catch (_: Exception) {}
        }
        handler.removeCallbacksAndMessages(null)
        exec.shutdownNow()
        super.onDestroy()
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode == REQ_MIC) {
            if (grantResults.isNotEmpty() && grantResults[0] == PackageManager.PERMISSION_GRANTED) {
                startRecorder()
                status2("listening — speak; 'Hang up' ends the call")
            } else {
                typedMode("mic permission denied — typed text mode")
            }
        }
    }

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()

    /** A failure [Result] with the message as the exception. */
    private fun <T> resFail(msg: String): Result<T> =
        Result.failure(IllegalArgumentException(msg))
}
