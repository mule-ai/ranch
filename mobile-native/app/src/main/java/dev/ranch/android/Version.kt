package dev.ranch.android

import okhttp3.OkHttpClient
import okhttp3.Request
import java.util.concurrent.TimeUnit

/**
 * Update check — port of mobile/lib/version.ts. Compares this build's
 * baked version ("main-<sha>", set by release.yml, same string as
 * versions.json) with the latest published release in ranch-dist.
 * Local dev builds ("dev") skip the banner.
 */
object Version {
    /** Baked at build time via the RANCH_VERSION gradle env; "dev" locally. */
    val own: String = BuildConfig.RANCH_VERSION

    data class Update(val latest: String, val apkUrl: String)

    private val client = OkHttpClient.Builder()
        .connectTimeout(10, TimeUnit.SECONDS)
        .readTimeout(10, TimeUnit.SECONDS)
        .build()

    /** null = up-to-date, dev build, or fetch failure (all silent). */
    fun checkUpdate(): Update? {
        if (own.isEmpty() || own == "dev") return null
        return try {
            val req = Request.Builder()
                .url("https://raw.githubusercontent.com/mule-ai/ranch-dist/main/versions.json")
                .get()
                .build()
            client.newCall(req).execute().use { resp ->
                if (!resp.isSuccessful) return null
                val body = resp.body?.string() ?: return null
                val latest = org.json.JSONObject(body).optString("version", "")
                if (latest.isEmpty() || latest == own) null
                else Update(
                    latest,
                    "https://raw.githubusercontent.com/mule-ai/ranch-dist/main/ranch.apk",
                )
            }
        } catch (_: Exception) { null }
    }
}
