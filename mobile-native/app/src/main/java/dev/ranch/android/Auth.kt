package dev.ranch.android

import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONArray
import org.json.JSONObject
import java.util.concurrent.TimeUnit
import kotlin.concurrent.Volatile

/**
 * Supabase GoTrue auth (password grant + refresh) and machine listing
 * over PostgREST. Tokens persist in SharedPreferences so the monitor
 * service can run without the UI.
 */
class Auth(private val prefs: Prefs) {
    private val client = OkHttpClient.Builder()
        .connectTimeout(15, TimeUnit.SECONDS)
        .readTimeout(15, TimeUnit.SECONDS)
        .build()

    @Volatile
    var accessToken: String = prefs.get("access_token", "")
        private set
    @Volatile
    var refreshToken: String = prefs.get("refresh_token", "")
        private set
    @Volatile
    var expiresAt: Long = prefs.get("expires_at", "0").toLong()

    fun isLoggedIn(): Boolean = accessToken.isNotEmpty()

    /** @return null on success, error string otherwise. */
    fun login(email: String, password: String): String? {
        val body = JSONObject()
            .put("email", email.trim())
            .put("password", password)
            .toString()
        return doAuthRequest("password", body)
    }

    /**
     * Phase 5 — Google OAuth (browser flow).
     * 1. Open [googleAuthorizeUrl] in the system browser / custom tab.
     * 2. Supabase completes the Google dance and redirects to
     *    `ranch://auth-callback#access_token=…&refresh_token=…`.
     * 3. The manifest intent-filter routes that to MainActivity, which
     *    hands the URI here: [applyOAuthFragment] persists the tokens.
     * Requires the Google provider to be enabled in the Supabase dashboard
     * with `ranch://auth-callback` in its redirect-allow list.
     */
    fun googleAuthorizeUrl(): String {
        val redirect = "ranch://auth-callback"
        return "${Supabase.URL}/auth/v1/authorize?provider=google" +
            "&redirect_to=" + android.net.Uri.encode(redirect)
    }

    /** @return null on success, error string otherwise. */
    fun applyOAuthFragment(fragment: String?): String? {
        if (fragment.isNullOrEmpty()) return "empty redirect"
        val parts = fragment
            .split("&")
            .mapNotNull {
                val i = it.indexOf('=')
                if (i <= 0) null else
                    java.net.URLDecoder.decode(it.substring(0, i), "UTF-8") to
                    java.net.URLDecoder.decode(it.substring(i + 1), "UTF-8")
            }
            .toMap()
        val at = parts["access_token"]
            ?: return "no access_token in redirect (error: ${parts["error_description"] ?: parts["error"] ?: "?"})"
        val rt = parts["refresh_token"] ?: return "no refresh_token in redirect"
        val expiresIn = parts["expires_in"]?.toLongOrNull() ?: 3600L
        accessToken = at
        refreshToken = rt
        expiresAt = System.currentTimeMillis() + expiresIn * 1000L
        prefs.set("access_token", accessToken)
        prefs.set("refresh_token", refreshToken)
        prefs.set("expires_at", expiresAt.toString())
        return null
    }

    /** @return null on success, error string otherwise. */
    fun refresh(): String? =
        if (refreshToken.isEmpty()) "no refresh token"
        else doAuthRequest("refresh_token", JSONObject().put("refresh_token", refreshToken).toString())

    private fun doAuthRequest(grant: String, body: String): String? {
        return try {
            val req = Request.Builder()
                .url("${Supabase.URL}/auth/v1/token?grant_type=$grant")
                .header("apikey", Supabase.ANON)
                .header("Content-Type", "application/json")
                .post(body.toRequestBody("application/json".toMediaType()))
                .build()
            client.newCall(req).execute().use { resp ->
                val text = resp.body?.string() ?: ""
                if (!resp.isSuccessful) return "auth $grant failed: ${resp.code} $text"
                applyToken(JSONObject(text))
                null
            }
        } catch (e: Exception) { "auth $grant error: ${e.message}" }
    }

    private fun applyToken(j: JSONObject) {
        accessToken = j.optString("access_token", "")
        if (j.has("refresh_token")) refreshToken = j.getString("refresh_token")
        val expiresIn = j.optLong("expires_in", 3600)
        expiresAt = System.currentTimeMillis() + expiresIn * 1000L
        prefs.set("access_token", accessToken)
        prefs.set("refresh_token", refreshToken)
        prefs.set("expires_at", expiresAt.toString())
    }

    /** Machines the signed-in user can see. @return null on error. */
    fun machines(): Result<List<Machine>> = try {
        val req = Request.Builder()
            .url("${Supabase.URL}/rest/v1/machines_info?select=id,name,last_seen_at&order=name")
            .header("apikey", Supabase.ANON)
            .header("Authorization", "Bearer $accessToken")
            .get()
            .build()
        client.newCall(req).execute().use { resp ->
            val text = resp.body?.string() ?: "[]"
            if (!resp.isSuccessful) return Result.failure(Exception("${resp.code} $text"))
            val arr = JSONArray(text)
            Result.success(
                (0 until arr.length()).map { i ->
                    val o = arr.getJSONObject(i)
                    Machine(o.getString("id"), o.optString("name"), o.optString("last_seen_at"))
                }
            )
        }
    } catch (e: Exception) {
        Result.failure(e)
    }
}
