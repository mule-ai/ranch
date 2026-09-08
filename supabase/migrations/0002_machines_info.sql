-- Ranch: machine view for API clients (supabase-js select(*) friendly)
--
-- The `machines` table keeps key_hash out of API reads via column grants,
-- which makes select(*) error instead of omitting. This security_barrier
-- view exposes everything except key_hash; RLS on the base table still
-- applies with the caller's privileges.

create or replace view machines_info with (security_barrier = on) as
  select id, user_id, name, created_at, last_seen_at
  from machines;
