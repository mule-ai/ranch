// Update check: compare this build's version (baked in at CI build
// time, e.g. "main-82c0643" — the same string as versions.json) with
// the latest published release. Local dev builds have no baked version
// ("dev") — skip the banner, a dev checkout is expected to be ahead.
import { fetchLatest } from "./releases";

// baked by web.yml (VITE_APP_VERSION); undefined in local dev
const own = (import.meta.env.VITE_APP_VERSION as string | undefined) ?? "";

export type UpdateInfo = {
  latest: string;
  /** absolute APK download url (the phone-side update path) */
  apkUrl: string;
};

export async function checkUpdate(): Promise<UpdateInfo | null> {
  if (!own || own === "dev") return null;
  const latest = await fetchLatest();
  if (!latest?.version || latest.version === own) return null;
  return { latest: latest.version, apkUrl: distApkUrl() };
}

// the hosted site links the APK by name from the dist repo
function distApkUrl(): string {
  return "https://raw.githubusercontent.com/mule-ai/ranch-dist/main/ranch.apk";
}

export const ownVersion = own;
