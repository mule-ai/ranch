// Update check: compare this build's version (baked in at CI build
// time, e.g. "main-82c0643" — same string as versions.json) with the
// latest published release. Local dev builds have no baked version —
// skip the banner.
// baked by release.yml (EXPO_PUBLIC_APP_VERSION); undefined locally
const own = process.env.EXPO_PUBLIC_APP_VERSION ?? "";

export type UpdateInfo = { latest: string; apkUrl: string };

export async function checkUpdate(): Promise<UpdateInfo | null> {
  if (!own || own === "dev") return null;
  const r = await fetch(
    "https://raw.githubusercontent.com/mule-ai/ranch-dist/main/versions.json",
    { cache: "no-store" }
  );
  if (!r.ok) return null;
  const latest = (await r.json()) as { version?: string };
  if (!latest.version || latest.version === own) return null;
  return {
    latest: latest.version,
    apkUrl: "https://raw.githubusercontent.com/mule-ai/ranch-dist/main/ranch.apk",
  };
}

export const ownVersion = own;
