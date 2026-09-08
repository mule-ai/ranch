-- Ranch: self-service machine registration (multi-user, OAuth era).
--
-- With Google OAuth sign-in, there is no service role on user devices and
-- no admin env file. A logged-in user registers a machine by calling
-- register_machine() — a security-definer RPC that:
--   1. mints a machine key (pgcrypto, shown once to the caller)
--   2. creates the machine's Supabase Auth user (password = machine key,
--      user_metadata.machine_id = machines.id — same shape as migration
--      0001 so the channel RLS from 0003 keeps working unchanged)
--   3. inserts the machines row owned by the caller
-- Registration is how a daemon host joins your account; the daemon then
-- authenticates as the machine user (its key) for Realtime + REST.

create or replace function public.register_machine(p_name text)
returns table (machine_id uuid, machine_key text)
language plpgsql
security definer
set search_path = public, extensions
as $$
declare
  v_uid uuid := auth.uid();
  v_id uuid := gen_random_uuid();
  v_email text := 'machine-' || v_id || '@ranch.local';
  v_key text := encode(extensions.gen_random_bytes(24), 'hex');
  v_user_id uuid;
begin
  if v_uid is null then
    raise exception 'not authenticated';
  end if;
  if p_name is null or length(trim(p_name)) < 1 or length(p_name) > 64 then
    raise exception 'machine name must be 1..64 chars';
  end if;

  -- machine auth user (bcrypt hash in GoTrue's expected format)
  insert into auth.users (
    instance_id, id, aud, role, email,
    encrypted_password, email_confirmed_at,
    raw_app_meta_data, raw_user_meta_data,
    created_at, updated_at, confirmation_token, recovery_token,
    email_change_token_new, email_change
  ) values (
    '00000000-0000-0000-0000-000000000000', gen_random_uuid(),
    'authenticated', 'authenticated', v_email,
    extensions.crypt(v_key, extensions.gen_salt('bf')),
    now(),
    '{"provider":"email","providers":["email"]}',
    jsonb_build_object('machine_id', v_id::text),
    now(), now(), '', '', '', ''
  ) returning id into v_user_id;

  insert into public.machines (id, user_id, key_hash, name)
  values (v_id, v_uid, encode(extensions.digest(v_key, 'sha256'), 'hex'), trim(p_name));

  return query select v_id, v_key;
end;
$$;

revoke all on function public.register_machine(text) from public, anon;
grant execute on function public.register_machine(text) to authenticated;

-- unregister: deletes the machines row (auth user row is left; the
-- machine key no longer grants anything once the row is gone).
create or replace function public.unregister_machine(p_machine_id uuid)
returns void
language plpgsql
security definer
set search_path = public
as $$
begin
  if auth.uid() is null then
    raise exception 'not authenticated';
  end if;
  delete from public.machines
  where id = p_machine_id and user_id = auth.uid();
end;
$$;

revoke all on function public.unregister_machine(uuid) from public, anon;
grant execute on function public.unregister_machine(uuid) to authenticated;
