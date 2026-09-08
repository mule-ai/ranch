-- Ranch: unregister_machine also removes the machine's auth user so
-- re-registering with the same name doesn't collide and orphans don't
-- accumulate in auth.users.

create or replace function public.unregister_machine(p_machine_id uuid)
returns void
language plpgsql
security definer
set search_path = public
as $$
declare
  v_email text;
begin
  if auth.uid() is null then
    raise exception 'not authenticated';
  end if;
  select 'machine-' || id || '@ranch.local' into v_email
  from public.machines
  where id = p_machine_id and user_id = auth.uid();
  if not found then
    return; -- not yours / doesn't exist; idempotent no-op
  end if;
  delete from public.machines where id = p_machine_id;
  delete from auth.users where email = v_email;
end;
$$;
