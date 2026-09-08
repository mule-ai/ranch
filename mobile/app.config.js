// Baked-in defaults (override via env at build time, mirroring the CLI).
const DEFAULT_URL = process.env.EXPO_PUBLIC_SUPABASE_URL || "https://prqfseydoxyingbkmiic.supabase.co";
const DEFAULT_ANON = process.env.EXPO_PUBLIC_SUPABASE_ANON_KEY || "";

export default {
  name: "Ranch",
  slug: "ranch",
  version: "0.1.0",
  orientation: "default",
  userInterfaceStyle: "dark",
  scheme: "ranch",
  backgroundColor: "#101014",
  android: { package: "dev.ranch.app", versionCode: 1 },
  extra: { supabaseUrl: DEFAULT_URL, anonKey: DEFAULT_ANON },
};
