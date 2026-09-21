package dev.ranch.android

import android.content.Context
import android.content.SharedPreferences

/** Thin SharedPreferences wrapper for auth tokens + settings. */
class Prefs(context: Context) {
    private val sp: SharedPreferences =
        context.applicationContext.getSharedPreferences("ranch-native", Context.MODE_PRIVATE)

    fun get(key: String, default: String): String = sp.getString(key, default) ?: default
    fun set(key: String, value: String) {
        sp.edit().putString(key, value).apply()
    }
    fun getBool(key: String, default: Boolean): Boolean = sp.getBoolean(key, default)
    fun setBool(key: String, value: Boolean) {
        sp.edit().putBoolean(key, value).apply()
    }
}
