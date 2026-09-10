-- SECURITY FIX: `machines_info` was created with security_barrier only,
-- which does NOT change privilege context — by default a Postgres view
-- executes with its OWNER's rights, so RLS on `machines` never applied
-- and every authenticated user could list ALL machines (machines are
-- how the web/mobile clients render "online machines").
--
-- security_invoker=true makes the view execute with the CALLER's
-- privileges, so the base-table RLS (owner sees own machines, machine
-- user sees its own row) applies.
alter view machines_info set (security_invoker = true);
