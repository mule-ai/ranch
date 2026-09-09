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
  android: {
    package: "dev.ranch.app",
    versionCode: 1,
    // resize (not pan) so the window shrinks when the keyboard shows;
    // combined with the in-app keyboard inset the bottom inputs and the
    // terminal's bottom line stay visible above the keyboard
    softwareKeyboardLayoutMode: "resize",
  },
  extra: { supabaseUrl: DEFAULT_URL, anonKey: DEFAULT_ANON },
};
