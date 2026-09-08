-- Ranch: cloud registry + relay gating
-- Supabase migration 0001 (applied to project `ranch`, ref prqfseydoxyingbkmiic)
--
-- Identity model:
--   * owner  — a normal Supabase Auth user (email/OAuth). Owns machines.
--   * machine — a dedicated Supabase Auth user per machine. Its JWT carries
--               user_metadata.machine_id = machines.id, which is how RLS
--               recognizes it. The machine's password is the "machine key"
--               (shown once at `ranch register`, stored 0600 on the host).
--
-- Terminal frames flow over a private Realtime broadcast channel per machine
-- (topic `machines:<machine_id>`); the `machines` table is in the
-- supabase_realtime publication so Realtime can RLS-gate channel joins.

create table if not exists machines (
  id           uuid primary key,
  user_id      uuid not null references auth.users on delete cascade,
  key_hash     text not null unique,   -- sha256(machine key); key shown once
  name         text not null,
  created_at   timestamptz not null default now(),
  last_seen_at timestamptz
);

create table if not exists sessions (   -- registry mirror; truth lives in ranchd
  id             uuid primary key,      -- daemon-assigned session id
  machine_id     uuid not null references machines on delete cascade,
  name           text not null,
  kind           text not null default 'shell',   -- shell | forge | mule
  ref_id         text,                    -- forge session id / mule workflow id
  created_at     timestamptz not null default now(),
  last_active_at timestamptz
);

create index if not exists sessions_machine_id on sessions (machine_id);
create index if not exists machines_user_id on machines (user_id);

-- ---------- RLS ----------
alter table machines enable row level security;
alter table sessions enable row level security;

-- owner: full access to own machines / their sessions
create policy "machines_owner_all" on machines for all
  using (auth.uid() = user_id)
  with check (auth.uid() = user_id);

create policy "sessions_owner_all" on sessions for all
  using (exists (
    select 1 from machines m
    where m.id = sessions.machine_id and m.user_id = auth.uid()))
  with check (exists (
    select 1 from machines m
    where m.id = sessions.machine_id and m.user_id = auth.uid()));

-- machine: read + update (heartbeat) own row, write own sessions
create policy "machines_machine_select" on machines for select
  using ((auth.jwt() -> 'user_metadata' ->> 'machine_id') = id::text);

create policy "machines_machine_update" on machines for update
  using ((auth.jwt() -> 'user_metadata' ->> 'machine_id') = id::text)
  with check ((auth.jwt() -> 'user_metadata' ->> 'machine_id') = id::text);

create policy "sessions_machine_all" on sessions for all
  using ((auth.jwt() -> 'user_metadata' ->> 'machine_id') = machine_id::text)
  with check ((auth.jwt() -> 'user_metadata' ->> 'machine_id') = machine_id::text);

-- key_hash is never readable via the API: drop the default table grants and
-- re-grant a column list that excludes it. (service_role keeps it — the
-- owner's admin path.)
revoke select on machines from anon, authenticated;
grant select (id, user_id, name, created_at, last_seen_at) on machines to authenticated;

revoke select on sessions from anon;
grant select on sessions to authenticated;

-- Realtime: allow RLS-gated private channels keyed on machines rows
alter publication supabase_realtime add table machines;
