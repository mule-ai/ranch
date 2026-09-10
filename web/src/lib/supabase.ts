// Supabase client for the web app. Same project as the mobile app; the
// anon key is public by design (RLS governs data access). Sessions
// persist in localStorage; the OAuth redirect comes back to this page.
import { createClient } from "@supabase/supabase-js";

const SUPABASE_URL =
  (import.meta as unknown as { env: Record<string, string> }).env.EXPO_PUBLIC_SUPABASE_URL ??
  "https://prqfseydoxyingbkmiic.supabase.co";
const SUPABASE_ANON_KEY =
  (import.meta as unknown as { env: Record<string, string> }).env.EXPO_PUBLIC_SUPABASE_ANON_KEY ??
  "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6InBycWZzZXlkb3h5aW5nYmttaWljIiwicm9sZSI6ImFub24iLCJpYXQiOjE3ODg4NDk2NzQsImV4cCI6MjEwNDQyNTY3NH0.lGEKMCE_dWvIDrkXjdXz3KTZtC7Nbd9EtBDSmRYJ3mU";

export const supabase = createClient(SUPABASE_URL, SUPABASE_ANON_KEY, {
  auth: {
    // default localStorage storage; detectSessionInUrl picks up the
    // OAuth ?code= redirect when the app loads at /ranch/
    detectSessionInUrl: true,
    flowType: "pkce",
  },
});
