// Baked-in defaults (override via env at build time, mirroring the CLI).
const DEFAULT_URL = process.env.EXPO_PUBLIC_SUPABASE_URL || "https://prqfseydoxyingbkmiic.supabase.co";
const DEFAULT_ANON = process.env.EXPO_PUBLIC_SUPABASE_ANON_KEY || "";

export default {
  name: "Ranch",
  slug: "ranch",
  version: "0.1.1",
  orientation: "default",
  userInterfaceStyle: "dark",
  scheme: "ranch",
  backgroundColor: "#101014",
  icon: "./assets/icon/icon-1024.png",
  android: {
    package: "dev.ranch.app",
    versionCode: 2,
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
