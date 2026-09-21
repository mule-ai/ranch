package dev.ranch.android

import okio.ByteString
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import org.json.JSONArray
import org.json.JSONObject
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import kotlin.concurrent.Volatile

/**
 * Supabase Realtime (Phoenix) client — the native port of
 * `crates/ranch/src/relay.rs`'s client-side join/heartbeat logic and
 * `mobile/lib/relay.ts`'s frame dispatch.
 *
 * Topic: `realtime:machines:{machineId}` (private channel, JWT in the
 * join payload). Frames arrive as broadcast events:
 *   {"topic":..., "event":"broadcast",
 *    "payload":{"event":"frame", "payload":{...ranch frame...}}}
 *
 * Liveness: 25 s app-level heartbeats; 75 s with no server traffic is a
 * zombie connection → full reconnect + re-join. JWT is refreshed ~2 min
 * before expiry (access_token event + re-join).
 */
class Realtime(
    private val machineId: String,
    private val auth: Auth,
    private val onFrame: (JSONObject) -> Unit,
    private val onStatus: (String) -> Unit,
) {
    private val topic = "realtime:machines:$machineId"
    private val client = OkHttpClient.Builder()
        .pingInterval(0, TimeUnit.SECONDS) // we do app-level heartbeat instead
        .readTimeout(0, TimeUnit.SECONDS)  // no read timeout; zombie guard handles it
        .build()
    private val refCounter = AtomicInteger(1)
    @Volatile private var ws: WebSocket? = null
    @Volatile private var running = false
    @Volatile private var joinOk = false
    @Volatile private var lastServerSeen = 0L
    @Volatile private var lastHbSent = 0L
    @Volatile private var lastSeenUpd = 0L
    @Volatile private var tokenDeadline = 0L
    @Volatile private var refreshFailures = 0
    private var pumpThread: Thread? = null

    private data class Chunk(
        val n: Int,
        val parts: Array<String?>,
        @Volatile var received: Int = 0,
    )
    private val chunks = HashMap<String, Chunk>()

    fun start() {
        running = true
        connect()
        pumpThread = Thread({ pumpLoop() }, "ranch-realtime-pump").also { it.start() }
    }

    fun stop() {
        running = false
        try { ws?.close(1000, "stop") } catch (_: Exception) {}
        ws = null
        pumpThread?.let {
            it.interrupt()
            try { it.join(3000) } catch (_: InterruptedException) {}
        }
        pumpThread = null
    }

    // ---- connection lifecycle ----

    private fun connect() {
        if (!running) return
        joinOk = false
        lastServerSeen = System.currentTimeMillis()
        lastHbSent = lastServerSeen
        lastSeenUpd = lastServerSeen
        ensureFreshToken()
        tokenDeadline = auth.expiresAt - 120_000L
        onStatus("connecting")
        val url = "${Supabase.realtimeWsUrl()}"
        val req = Request.Builder().url(url).build()
        ws = client.newWebSocket(req, listener)
    }

    private fun ensureFreshToken() {
        synchronized(auth) {
            if (System.currentTimeMillis() >= auth.expiresAt - 120_000L &&
                auth.refreshToken.isNotEmpty()
            ) {
                val err = auth.refresh()
                if (err != null) {
                    onStatus("refresh failed: $err")
                }
            }
        }
    }

    private val listener = object : WebSocketListener() {
        override fun onOpen(webSocket: WebSocket, response: Response) {
            lastServerSeen = System.currentTimeMillis()
            sendJoin()
            onStatus("online")
        }

        override fun onMessage(webSocket: WebSocket, text: String) {
            lastServerSeen = System.currentTimeMillis()
            handleText(text)
        }

        override fun onMessage(webSocket: WebSocket, bytes: ByteString) {
            lastServerSeen = System.currentTimeMillis()
            try {
                handleText(bytes.utf8())
            } catch (_: Exception) {}
        }

        override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
            webSocket.close(code, reason)
        }

        override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
            if (running) { onStatus("reconnecting"); reconnect() }
        }

        override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
            if (running) { onStatus("reconnecting"); reconnect() }
        }
    }

    private fun reconnect() {
        synchronized(this) {
            if (!running) return
            try { ws?.close(1000, "reconnect") } catch (_: Exception) {}
            ws = null
            joinOk = false
            try { Thread.sleep(1500) } catch (_: InterruptedException) {}
            connect()
        }
    }

    // ---- pump: heartbeat + token refresh + zombie detection ----

    private fun pumpLoop() {
        while (running) {
            try { Thread.sleep(1000) } catch (_: InterruptedException) { break }
            if (!running) break
            val now = System.currentTimeMillis()
            if (ws == null || !joinOk) continue

            // heartbeat
            if (now - lastHbSent > 25_000) {
                sendHeartbeat()
                lastHbSent = now
            }

            // token refresh
            var doReconnect = false
            synchronized(auth) {
                if (now >= auth.expiresAt - 120_000L && auth.refreshToken.isNotEmpty()) {
                    val err = auth.refresh()
                    if (err == null) {
                        refreshFailures = 0
                        sendAccessToken()
                        sendJoin()
                        onStatus("token refreshed")
                    } else {
                        refreshFailures++
                        onStatus("refresh failed: $err")
                        if (refreshFailures >= 3) {
                            refreshFailures = 0
                            doReconnect = true
                        }
                    }
                }
            }
            if (doReconnect) {
                onStatus("token refresh failed 3x — reconnecting")
                reconnect()
                continue
            }

            // zombie guard
            if (now - lastServerSeen > 75_000) {
                onStatus("zombie detected — reconnecting")
                reconnect()
            }
        }
    }

    // ---- send helpers ----

    private fun sendJoin() {
        synchronized(auth) {
            val payload = JSONObject()
                .put("config", JSONObject()
                    .put("broadcast", JSONObject())
                    .put("presence", JSONObject())
                    .put("postgres_changes", JSONArray())
                    .put("private", true))
                .put("access_token", auth.accessToken)
            val msg = JSONObject()
                .put("topic", topic)
                .put("event", "phx_join")
                .put("ref", "join")
                .put("payload", payload)
            ws?.send(msg.toString())
        }
    }

    private fun sendHeartbeat() {
        val msg = JSONObject()
            .put("topic", "phoenix")
            .put("event", "phx_heartbeat")
            .put("payload", JSONObject())
            .put("ref", "hb${refCounter.incrementAndGet()}")
        ws?.send(msg.toString())
    }

    private fun sendAccessToken() {
        synchronized(auth) {
            val msg = JSONObject()
                .put("topic", "phoenix")
                .put("event", "access_token")
                .put("payload", JSONObject().put("access_token", auth.accessToken))
                .put("ref", "tok${refCounter.incrementAndGet()}")
            ws?.send(msg.toString())
        }
    }

    // ---- incoming message handling ----

    private fun handleText(text: String) {
        try {
            val msg = JSONObject(text)
            val msgTopic = msg.optString("topic")
            val event = msg.optString("event")

            // Phoenix join/leave/heartbeat replies
            if (event == "phx_reply") {
                val ref = msg.optString("ref")
                val reply = msg.optJSONObject("reply")
                if (ref == "join" && reply != null &&
                    reply.optString("status") == "ok"
                ) {
                    joinOk = true
                    lastServerSeen = System.currentTimeMillis()
                }
                return
            }

            // server-side heartbeat ack
            if (event == "heartbeat") {
                lastServerSeen = System.currentTimeMillis()
                return
            }

            // broadcast frames
            if (event == "broadcast" && msgTopic == topic) {
                val payload = msg.optJSONObject("payload") ?: return
                if (payload.optString("event") != "frame") return
                val frame = payload.optJSONObject("payload") ?: return

                if (frame.optString("t") == "Chunk") {
                    handleChunk(frame)
                    return
                }
                onFrame(frame)
            }
        } catch (_: Exception) {
            // malformed frame — ignore
        }
    }

    private fun handleChunk(frame: JSONObject) {
        val cid = frame.optString("chunk_id")
        val i = frame.optInt("i")
        val n = frame.optInt("n")
        val data = frame.optString("data")
        var chunk = chunks[cid]
        if (chunk == null) {
            chunk = Chunk(n, Array(n) { null })
            chunks[cid] = chunk
        }
        if (i in 0 until chunk.n && chunk.parts[i] == null) {
            chunk.parts[i] = data
            chunk.received++
        }
        if (chunk.received == chunk.n) {
            chunks.remove(cid)
            try {
                val full = JSONObject(chunk.parts.joinToString("") { it ?: "" })
                onFrame(full)
            } catch (_: Exception) {
                // torn chunk — seq-gap / re-attach path will recover
            }
        }
    }
}
