// Releases index: fetched from the ranch-dist repo's versions.json at
// build+runtime (CI writes it after publishing artifacts). Falls back to
// the latest known-good local build info if the fetch fails.
export type Release = {
  version: string;
  built: string; // ISO date
  apk: string; // url
  linux: string; // url
  sha256_apk?: string;
  sha256_linux?: string;
};

export const DIST_URL =
  "https://raw.githubusercontent.com/mule-ai/ranch-dist/main/versions.json";

export async function fetchLatest(): Promise<Release | null> {
  try {
    const r = await fetch(DIST_URL, { cache: "no-store" });
    if (!r.ok) return null;
    return (await r.json()) as Release;
  } catch {
    return null;
  }
}
