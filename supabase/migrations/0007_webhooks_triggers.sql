-- Ranch: webhook receiver tables (Phase E).
--
-- The edge function (supabase/functions/webhook-relay) resolves
-- webhook_id -> machine and broadcasts the verified payload into the
-- machine's Realtime channel. Secrets are stored hashed (SHA-256);
-- the raw secret is shown ONCE at creation (same pattern as machine
-- keys). RLS: owner-only.

create table webhooks (
  id          uuid primary key default gen_random_uuid(),
  user_id     uuid not null references auth.users on delete cascade,
  machine_id  uuid not null references machines on delete cascade,
  name        text not null,
  secret_hash text not null,                 -- sha256 hex; raw shown once
  sources     text[] not null default '{}',  -- allowed source tags (empty = any)
  debug       boolean not null default false, -- log bodies (never secrets)
  created_at  timestamptz not null default now()
);

create table webhook_log (
  id          bigint generated always as identity primary key,
  webhook_id  uuid not null references webhooks on delete cascade,
  source      text,
  event       text,
  status      int not null,                 -- HTTP status we answered with
  received_at timestamptz not null default now()
);

create index webhook_log_webhook_idx on webhook_log (webhook_id, received_at desc);

-- Trigger registry mirror (Phase D): truth lives daemon-side; this row
-- exists so dashboards can show trigger state for offline machines.
create table triggers (
  id          uuid primary key,
  machine_id  uuid not null references machines on delete cascade,
  name        text not null,
  workflow_id text not null,
  kind        text not null check (kind in ('cron', 'event', 'webhook')),
  enabled     boolean not null default true,
  spec        jsonb not null default '{}',
  updated_at  timestamptz not null default now()
);

alter table webhooks  enable row level security;
alter table webhook_log enable row level security;
alter table triggers  enable row level security;

create policy "webhooks_owner" on webhooks
  for all to authenticated
  using (user_id = auth.uid())
  with check (user_id = auth.uid());

create policy "webhook_log_owner" on webhook_log
  for all to authenticated
  using (
    exists (select 1 from webhooks w where w.id = webhook_id and w.user_id = auth.uid())
  )
  with check (
    exists (select 1 from webhooks w where w.id = webhook_id and w.user_id = auth.uid())
  );

create policy "triggers_owner" on triggers
  for all to authenticated
  using (machine_id in (select m.id from machines m where m.user_id = auth.uid()))
  with check (machine_id in (select m.id from machines m where m.user_id = auth.uid()));

-- machines (the machine auth user) may update its own trigger/webhook
-- mirror rows via the REST proxy (machine JWT carries machine_id in
-- user_metadata).
create policy "triggers_machine" on triggers
  for all to authenticated
  using (machine_id::text = coalesce(auth.jwt() -> 'user_metadata' ->> 'machine_id', ''))
  with check (machine_id::text = coalesce(auth.jwt() -> 'user_metadata' ->> 'machine_id', ''));

create policy "webhooks_machine" on webhooks
  for select to authenticated
  using (machine_id::text = coalesce(auth.jwt() -> 'user_metadata' ->> 'machine_id', ''));


-- Secret at rest: the edge function needs the RAW secret to HMAC-verify
-- (a hash can't). Store it encrypted with pgp_sym_encrypt under a
-- secret held in vault; only the service role can decrypt. The column
-- below is named secret_hash for backward-compat with the design doc;
-- it holds pgp_sym_encrypt(secret, vault_secret).
create extension if not exists pgcrypto;

create or replace function public.store_webhook_secret(p_webhook_id uuid, p_secret text)
returns void
language sql
security definer
set search_path = public, vault, extensions
as $$
  update webhooks
  set secret_hash = pgp_sym_encrypt(
    p_secret,
    coalesce((select secret from vault.decrypted_secrets where name = 'webhook_secret_key'), 'ranch-webhook-secret')
  )
  where id = p_webhook_id;
$$;

create or replace function public.read_webhook_secret(p_webhook_id uuid)
returns text
language sql
security definer
set search_path = public, vault, extensions
as $$
  select pgp_sym_decrypt(
    secret_hash::bytea,
    coalesce((select secret from vault.decrypted_secrets where name = 'webhook_secret_key'), 'ranch-webhook-secret')
  )
  from webhooks where id = p_webhook_id;
$$;

revoke all on function public.store_webhook_secret(uuid, text) from public, anon, authenticated;
revoke all on function public.read_webhook_secret(uuid) from public, anon, authenticated;
-- service role (the edge function) keeps access via role bypass.
