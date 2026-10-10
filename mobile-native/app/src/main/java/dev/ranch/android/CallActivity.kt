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

/**
 * H6.3 — voice loop v1 (the "call" screen, the 📞 button on an agent row).
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
 *     `{"done":true}` = the call ended.
 *
 * Barge-in needs nothing here: mule's orchestrator VAD triggers on your
 * voice while the TTS is playing; this screen just keeps capturing.
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

        val base = app.prefs.get(PKEY_MULE_BASE, "").trim().trimEnd('/')
        val key = app.prefs.get(PKEY_MULE_KEY, "").trim()
        if (base.isEmpty() || key.isEmpty()) {
            status.text = "mule is not configured"
            status.setTextColor(0xFFF59E0B.toInt())
            root.addView(Button(this).apply {
                text = "Open settings"
                setOnClickListener {
                    startActivity(Intent(this@CallActivity, SettingsActivity::class.java))
                }
            })
            return
        }
        startCall(base, key)
    }

    // ------------------------------------------------------------------
    // call start: find the voice_call workflow, start the job, dial WS
    // ------------------------------------------------------------------

    private fun startCall(base: String, key: String) {
        exec.execute {
            val wfResult = findVoiceWorkflow(base, key, agentId)
            val found = wfResult.getOrNull()
            if (found == null) {
                val err = wfResult.exceptionOrNull()
                if (err != null) { fail(err.message ?: "workflow lookup failed"); return@execute }
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
            try {
                connectOut(base, jobId, ttsRate)
                connectIn(base, jobId)
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
        val wfs = muleGet(base, key, "/api/v1/workflows") ?: return resFail("list workflows")
        val arr = when (wfs) {
            is JSONObject -> wfs.optJSONArray("workflows") ?: wfs.optJSONArray("data")
            else -> null
        }
        if (arr == null) return resFail("list workflows: unexpected shape")
        for (i in 0 until arr.length()) {
            val wf = arr.getJSONObject(i)
            val wfId = wf.optString("id")
            if (wfId.isEmpty()) continue
            val steps = muleGet(base, key, "/api/v1/workflows/$wfId/steps") ?: continue
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
                else Result.success(JSONObject(text).optString("id"))
            }
        } catch (e: Exception) {
            resFail(e.message ?: "request failed")
        }
    }

    private fun muleGet(base: String, key: String, path: String): Any? = try {
        val req = Request.Builder()
            .url(base + path)
            .header("Authorization", "Bearer $key")
            .get()
            .build()
        http.newCall(req).execute().use { resp ->
            if (!resp.isSuccessful) return null
            val text = resp.body?.string().orEmpty()
            try {
                when {
                    text.startsWith("[") -> org.json.JSONArray(text)
                    text.startsWith("{") -> JSONObject(text)
                    else -> null
                }
            } catch (e: Exception) {
                null
            }
        }
    } catch (e: Exception) {
        null
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
            while (recRunning.get()) {
                val n = rec.read(buf, 0, buf.size)
                if (n <= 0) continue
                if (n < buf.size) System.arraycopy(buf, 0, buf, 0, n)
                ws?.send(ByteString.of(*buf.copyOf(n))) ?: run {
                    onCallDropped("in socket gone")
                    break
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
