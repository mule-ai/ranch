import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// GitHub Pages serves the site under /ranch/ — keep asset paths relative
// so the build works at any base path. No router library: a tiny
// hash-based router (App.tsx) avoids 404s on deep links under Pages.
export default defineConfig({
  base: "./",
  plugins: [react()],
  server: { port: 5180 },
});
