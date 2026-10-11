-- Ranch: voice gateways (Herd cloud-voice, option 2)
-- Supabase migration 0009
--
-- Per-user routing table for the `ranch-voice` edge function: which
-- forge/mule endpoints a signed-in user's phone voice calls go to, and
-- the API keys that gate them. The edge function verifies the caller's
-- Supabase JWT (Google/password login) and reads exactly ONE row — the
-- caller's own — so each user's requests are routed to their own lab
-- and no user can reach another user's endpoints. The keys live here
-- (owner-visible, RLS-scoped) so the phone never has to be handed the
-- raw forge/mule keys for cloud mode.
--
-- Values are the PUBLIC reachable URLs of the user's services (e.g. a
-- cloudflare-tunnel hostname for their lab), NOT private LAN IPs: the
-- edge function runs on the public internet.

create table if not exists voice_gateways (
  uid            uuid primary key references auth.users (id) on delete cascade,
  forge_base_url text not null,              -- e.g. https://voice-lab.example (public)
  forge_api_key  text not null,
  mule_base_url  text not null,
  mule_api_key   text not null,
  updated_at     timestamptz not null default now()
);

-- ---------- RLS ----------
alter table voice_gateways enable row level security;

-- The user manages their OWN gateway row only. No anon access; the
-- edge function calls in with the user's own JWT and lands under these
-- same policies (it never uses the service role for this table).
create policy "voice_gateways_owner_all" on voice_gateways for all
  using (uid = auth.uid())
  with check (uid = auth.uid());

-- Convenience for app settings / psql-as-user: upsert in one call.
-- SECURITY INVOKER so the RLS above still binds the row to auth.uid().
create or replace function upsert_voice_gateway(
  p_forge_base_url text,
  p_forge_api_key  text,
  p_mule_base_url  text,
  p_mule_api_key   text
) returns void
language sql
security invoker
as $$
  insert into voice_gateways (uid, forge_base_url, forge_api_key, mule_base_url, mule_api_key)
  values (auth.uid(), trim(p_forge_base_url), p_forge_api_key, trim(p_mule_base_url), p_mule_api_key)
  on conflict (uid) do update set
    forge_base_url = excluded.forge_base_url,
    forge_api_key  = excluded.forge_api_key,
    mule_base_url  = excluded.mule_base_url,
    mule_api_key   = excluded.mule_api_key,
    updated_at     = now();
$$;

revoke execute on function upsert_voice_gateway(text, text, text, text) from anon;
grant execute on function upsert_voice_gateway(text, text, text, text) to authenticated;
