package dev.ranch.android

import android.content.Context
import android.database.sqlite.SQLiteDatabase
import android.database.sqlite.SQLiteOpenHelper
import org.json.JSONArray
import org.json.JSONObject

/**
 * Persistent render mirror of the herd's visible state (phase 2,
 * 2026-11-06): chat rows, the agent registry, and (via Prefs) the
 * notification watermarks survive app restarts, so a cold start paints
 * instantly from local disk instead of waiting on a daemon round-trip
 * and a replay.
 *
 * This is a RENDER CACHE, not a second truth — forge keeps the log.
 * Rows key on (pane, seq) with seq monotonic-append-only, so every
 * write is an INSERT OR REPLACE and a daemon-originated reset frame
 * (bulk prelude, watch replacement, sticky-conversation re-mint)
 * replaces a pane's whole set atomically. Pruned to the newest
 * [KEEP_PER_PANE] rows per pane.
 *
 * Writes arrive on the Realtime fan-out thread (single source, same
 * path the Notify engine reads); reads run on UI threads under
 * SQLite's internal locking. Raw row JSON is stored verbatim so the
 * wire shape can grow fields without a schema migration.
 */
class Mirror private constructor(context: Context) :
    SQLiteOpenHelper(context.applicationContext, "ranch-mirror", null, 1) {

    companion object {
        @Volatile
        private var inst: Mirror? = null
        fun get(context: Context): Mirror =
            inst ?: synchronized(this) { inst ?: Mirror(context).also { inst = it } }

        /** newest rows kept per pane (older scroll back comes from the
         *  daemon's worker-backed deep paging, not the mirror) */
        private const val KEEP_PER_PANE = 800
    }

    override fun onCreate(db: SQLiteDatabase) {
        db.execSQL(
            "CREATE TABLE chat_rows(pane TEXT NOT NULL, seq INTEGER NOT NULL, " +
                "json TEXT NOT NULL, created TEXT, PRIMARY KEY(pane, seq))"
        )
        db.execSQL("CREATE TABLE agents(id TEXT PRIMARY KEY, json TEXT NOT NULL, fetched INTEGER NOT NULL)")
    }

    override fun onUpgrade(db: SQLiteDatabase, oldVersion: Int, newVersion: Int) {
        // the mirror self-heals from the daemon — drop & rebuild
        db.execSQL("DROP TABLE IF EXISTS chat_rows")
        db.execSQL("DROP TABLE IF EXISTS agents")
        onCreate(db)
    }

    /** Realtime fan-out sink: cache what the phone was already shown. */
    fun onFrame(f: JSONObject) {
        when (f.optString("t")) {
            "Chat" -> {
                val pane = f.optString("pane")
                if (pane.isEmpty()) return
                val msgs = f.optJSONArray("msgs") ?: return
                putChat(pane, msgs, f.optBoolean("reset"))
            }
            "ChatHistoryOk" -> {
                val pane = f.optString("pane")
                if (pane.isEmpty()) return
                f.optJSONArray("msgs")?.let { putChat(pane, it, false) }
            }
            "AgentListOk" -> f.optJSONArray("agents")?.let { putAgents(it) }
        }
    }

    fun putChat(pane: String, msgs: JSONArray, reset: Boolean) {
        if (msgs.length() == 0 && !reset) return
        val db = writableDatabase
        db.beginTransaction()
        try {
            if (reset) db.execSQL("DELETE FROM chat_rows WHERE pane=?", arrayOf(pane))
            val ins = db.compileStatement(
                "INSERT OR REPLACE INTO chat_rows(pane, seq, json, created) VALUES(?,?,?,?)"
            )
            for (i in 0 until msgs.length()) {
                val m = msgs.getJSONObject(i)
                val seq = m.optLong("seq", -1)
                if (seq < 0) continue
                ins.bindString(1, pane)
                ins.bindLong(2, seq)
                ins.bindString(3, m.toString())
                ins.bindString(4, m.optString("created_at", ""))
                ins.executeInsert()
            }
            db.execSQL(
                "DELETE FROM chat_rows WHERE pane=? AND seq < " +
                    "(SELECT MIN(seq) FROM (SELECT seq FROM chat_rows WHERE pane=? " +
                    "ORDER BY seq DESC LIMIT ?))",
                arrayOf(pane, pane, KEEP_PER_PANE.toString())
            )
            // global ceiling: dead panes must not leak rows forever
            db.execSQL(
                "DELETE FROM chat_rows WHERE rowid NOT IN " +
                    "(SELECT rowid FROM chat_rows ORDER BY rowid DESC LIMIT 25000)"
            )
            db.setTransactionSuccessful()
        } finally {
            db.endTransaction()
        }
    }

    /** Latest `limit` rows of a pane, oldest first (raw row JSON). */
    fun getChat(pane: String, limit: Int = 120): List<JSONObject> {
        val out = ArrayList<JSONObject>()
        readableDatabase.rawQuery(
            "SELECT json FROM chat_rows WHERE pane=? ORDER BY seq DESC LIMIT ?",
            arrayOf(pane, limit.toString())
        ).use { c ->
            while (c.moveToNext()) {
                try {
                    out.add(JSONObject(c.getString(0)))
                } catch (e: Exception) {
                }
            }
        }
        out.reverse()
        return out
    }

    fun putAgents(arr: JSONArray) {
        val db = writableDatabase
        val now = System.currentTimeMillis()
        db.beginTransaction()
        try {
            val ins = db.compileStatement("INSERT OR REPLACE INTO agents(id, json, fetched) VALUES(?,?,?)")
            for (i in 0 until arr.length()) {
                val a = arr.getJSONObject(i)
                val id = a.optString("id")
                if (id.isEmpty()) continue
                ins.bindString(1, id)
                ins.bindString(2, a.toString())
                ins.bindLong(3, now)
                ins.executeInsert()
            }
            // an agent absent from this snapshot is gone
            db.execSQL("DELETE FROM agents WHERE fetched < ?", arrayOf(now.toString()))
            db.setTransactionSuccessful()
        } finally {
            db.endTransaction()
        }
    }

    /** Cached registry + freshness (ms since last refresh; -1 = cold). */
    fun getAgents(): Pair<List<JSONObject>, Long> {
        val out = ArrayList<JSONObject>()
        var newest = -1L
        readableDatabase.rawQuery("SELECT json, fetched FROM agents ORDER BY id", null).use { c ->
            while (c.moveToNext()) {
                try {
                    out.add(JSONObject(c.getString(0)))
                } catch (e: Exception) {
                }
                newest = maxOf(newest, c.getLong(1))
            }
        }
        return out to (if (newest < 0) -1L else System.currentTimeMillis() - newest)
    }

    /** Session died / pane killed — the mirror forgets with it. */
    fun forget(pane: String) {
        writableDatabase.execSQL("DELETE FROM chat_rows WHERE pane=?", arrayOf(pane))
    }
}
