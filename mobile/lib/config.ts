// Public cloud defaults, baked in like the Rust client's option_env! defaults.
// The anon key is public by design (RLS governs data access).
export const SUPABASE_URL = process.env.EXPO_PUBLIC_SUPABASE_URL ?? "https://prqfseydoxyingbkmiic.supabase.co";
export const SUPABASE_ANON_KEY = process.env.EXPO_PUBLIC_SUPABASE_ANON_KEY ?? "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6InBycWZzZXlkb3h5aW5nYmttaWljIiwicm9sZSI6ImFub24iLCJpYXQiOjE3ODg4NDk2NzQsImV4cCI6MjEwNDQyNTY3NH0.lGEKMCE_dWvIDrkXjdXz3KTZtC7Nbd9EtBDSmRYJ3mU";
