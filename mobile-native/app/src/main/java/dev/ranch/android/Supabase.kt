package dev.ranch.android

/**
 * Supabase cloud endpoint + anon key. The anon key is public by design —
 * all data access is governed by Row-Level Security on the Supabase side.
 * Mirrors mobile/lib/config.ts.
 */
object Supabase {
    const val URL = "https://prqfseydoxyingbkmiic.supabase.co"
    const val ANON =
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6InBycWZzZXlkb3h5aW5nYmttaWljIiwicm9sZSI6ImFub24iLCJpYXQiOjE3ODg4NDk2NzQsImV4cCI6MjEwNDQyNTY3NH0.lGEKMCE_dWvIDrkXjdXz3KTZtC7Nbd9EtBDSmRYJ3mU"

    /** wss endpoint for the Realtime (Phoenix) socket. */
    fun realtimeWsUrl(): String =
        URL.replaceFirst("https://", "wss://")
            .replaceFirst("http://", "ws://") + "/realtime/v1?apikey=$ANON&vsn=1.0.0"
}
