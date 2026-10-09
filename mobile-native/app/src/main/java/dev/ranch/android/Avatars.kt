package dev.ranch.android

import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import java.io.File
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executors
import okhttp3.OkHttpClient
import okhttp3.Request
import org.json.JSONObject

/**
 * H6.1 — agent avatar download/cache for notification big-picture
 * style. Avatars come from the H1 agent registry (`AgentListOk`
 * `avatar_url`); they are downloaded to `cacheDir/avatars/<sha256(url)[:16]>`
 * and decoded to a Bitmap that lives in a small in-memory map.
 *
 * Deliberately plain OkHttp (already a dependency) — no new deps.
 * The cache dir is the right place: avatars are a display nicety and
 * may be wiped; the in-memory map is process-scoped (re-fetched after
 * a process kill).
 */
object Avatars {
    private val client = OkHttpClient()
    private val exec = Executors.newSingleThreadExecutor()
    private val bitmaps = ConcurrentHashMap<String, Bitmap>() // url -> bitmap
    private val inflight = ConcurrentHashMap<String, Boolean>()
    private var context: Context? = null

    /** Call once at process start (Monitor.start wires it). */
    fun init(ctx: Context) {
        context = ctx.applicationContext
    }

    /** The cached bitmap for a url, or null (caller posts without a
     *  picture; [ensure] will fill it in for the next notification). */
    fun bitmapFor(url: String): Bitmap? = url.takeIf { it.isNotEmpty() }?.let { bitmaps[it] }

    /** Fire-and-forget fetch+decode. Never throws, never blocks. */
    fun ensure(url: String) {
        if (url.isEmpty() || bitmaps.containsKey(url) || inflight.putIfAbsent(url, true) != null) return
        val ctx = context ?: return
        exec.execute {
            try {
                val c = ctx.cacheDir
                c.mkdirs()
                val file = File(c, NotifyPolicy.avatarFileName(url))
                if (!file.exists()) {
                    val req = Request.Builder().url(url).build()
                    client.newCall(req).execute().use { resp ->
                        if (!resp.isSuccessful) {
                            inflight.remove(url)
                            return@execute
                        }
                        val body = resp.body ?: return@execute
                        file.parentFile.mkdirs()
                        body.byteStream().use { ins ->
                            java.io.FileOutputStream(file).use { ins.copyTo(it) }
                        }
                    }
                }
                val b = BitmapFactory.decodeFile(file.absolutePath)
                if (b != null) bitmaps[url] = b
            } catch (_: Exception) {
                // best-effort; a broken avatar URL never matters enough
                // to crash or surface
            } finally {
                inflight.remove(url)
            }
        }
    }

    /** Prefetch every known agent avatar (AgentListOk warm-up). */
    fun prefetch(agents: Array<String>) = agents.forEach { ensure(it) }

    /** Parse the per-agent prefs JSON `{name: {muted, priority}}` —
     *  tolerant of corrupt/absent input (returns an empty map). */
    fun parseAgentPrefs(json: String): Map<String, NotifyPolicy.AgentPrefs> {
        val out = HashMap<String, NotifyPolicy.AgentPrefs>()
        if (json.isBlank()) return out
        try {
            val o = JSONObject(json)
            for (k in o.keys()) {
                val p = o.optJSONObject(k) ?: continue
                out[k] = NotifyPolicy.AgentPrefs(
                    muted = p.optBoolean("muted", false),
                    priority = p.optString("priority", "normal").let {
                        if (it in setOf("high", "normal", "quiet")) it else "normal"
                    },
                )
            }
        } catch (_: Exception) {
        }
        return out
    }
}
