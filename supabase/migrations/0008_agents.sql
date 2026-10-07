-- Ranch: agent registry mirror (Herd H1.4)
-- Supabase migration 0008
--
-- The agent registry is owned by forge (the `agents` table there is
-- truth); this mirror lets a phone on another network see the registry
-- even when the machine's relay is offline. ranchd upserts its agents
-- here whenever it learns/updates the registry (AgentListOk pass-through
-- and agent_talk), mirroring the `sessions` mirror convention from
-- 0001. RLS: authenticated reads; writes only via the machine key.

create table if not exists agents (
  id             text primary key,      -- forge agent id (uuid string)
  machine_id     uuid not null references machines on delete cascade,
  name           text not null,
  avatar_url     text,
  home_machine   text,                   -- ranch machine the agent lives on (null = unbound)
  visibility     text,                   -- forge visibility ("private" | "public"); null when unknown
  created_at     timestamptz not null default now(),
  last_active_at timestamptz
);

create index if not exists agents_machine_id on agents (machine_id);

-- ---------- RLS (mirror 0001) ----------
alter table agents enable row level security;

-- owner: read + write the agents mirrored by own machines
create policy "agents_owner_all" on agents for all
  using (exists (
    select 1 from machines m
    where m.id = agents.machine_id and m.user_id = auth.uid()))
  with check (exists (
    select 1 from machines m
    where m.id = agents.machine_id and m.user_id = auth.uid()));

-- machine: full access to its own agent rows
create policy "agents_machine_all" on agents for all
  using ((auth.jwt() -> 'user_metadata' ->> 'machine_id') = machine_id::text)
  with check ((auth.jwt() -> 'user_metadata' ->> 'machine_id') = machine_id::text);

revoke select on agents from anon;
grant select on agents to authenticated;
