// Public demo account. Baked in at build time for the hosted demo build
// (VITE_DEMO_EMAIL / VITE_DEMO_PASSWORD). The demo account only owns the
// public demo machine, so leaking the credentials exposes nothing beyond
// the demo itself. The button only appears when both are set.
const email = import.meta.env.VITE_DEMO_EMAIL as string | undefined;
const password = import.meta.env.VITE_DEMO_PASSWORD as string | undefined;

export const demoAvailable = !!(email && password);

export async function signInDemo(): Promise<void> {
  if (!email || !password) throw new Error("demo not configured");
  const { supabase } = await import("./supabase");
  const { error } = await supabase.auth.signInWithPassword({ email, password });
  if (error) throw error;
}
