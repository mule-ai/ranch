// Baked-in defaults (override via env at build time, mirroring the CLI).
const DEFAULT_URL = process.env.EXPO_PUBLIC_SUPABASE_URL || "https://prqfseydoxyingbkmiic.supabase.co";
const DEFAULT_ANON = process.env.EXPO_PUBLIC_SUPABASE_ANON_KEY || "";

export default {
  name: "Ranch",
  slug: "ranch",
  version: "0.1.6",
  orientation: "default",
  userInterfaceStyle: "dark",
  scheme: "ranch",
  backgroundColor: "#101014",
  icon: "./assets/icon/icon-1024.png",
  android: {
    package: "dev.ranch.app",
    versionCode: 4,
    // Android 13+ (API 33+) runtime permission for local notifications.
    // Declared here (not just in the generated manifest) so `expo prebuild`
    // always emits it into android/app/src/main/AndroidManifest.xml.
    permissions: ["android.permission.POST_NOTIFICATIONS"],
    // resize (not pan) so the window shrinks when the keyboard shows;
    // combined with the in-app keyboard inset the bottom inputs and the
    // terminal's bottom line stay visible above the keyboard
    softwareKeyboardLayoutMode: "resize",
    adaptiveIcon: {
      // mule head + terminal cursor muzzle (assets/icon/*.svg sources)
      foregroundImage: "./assets/icon/icon-fg-1024.png",
      monochromeImage: "./assets/icon/icon-mono-1024.png",
      backgroundColor: "#101014",
    },
  },

  extra: { supabaseUrl: DEFAULT_URL, anonKey: DEFAULT_ANON },
};
