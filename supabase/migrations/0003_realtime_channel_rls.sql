-- Ranch: RLS policies for private Realtime broadcast channels.
--
-- Supabase Realtime gates private channels by probing the
-- `realtime.messages` table: on join it inserts a probe row with
-- topic = <sub_topic> (the channel topic without the `realtime:`
-- prefix) and SELECTs it back, all under the joining user's role.
-- RLS on `realtime.messages` therefore decides who may join (read)
-- and who may broadcast (write) a channel.
--
-- Our channel per machine is `realtime:machines:<machine_id>`, i.e.
-- sub_topic / messages.topic = `machines:<machine_id>`.
--   * the machine itself (JWT carries user_metadata.machine_id)
--   * the owner (machines.user_id = auth.uid())
-- may both read and write.

create policy "ranch_channel_read" on realtime.messages
  for select to authenticated
  using (
    topic = 'machines:' || coalesce(auth.jwt() -> 'user_metadata' ->> 'machine_id', '')
    or topic in (
      select 'machines:' || m.id::text
      from machines m
      where m.user_id = auth.uid()
    )
  );

create policy "ranch_channel_write" on realtime.messages
  for insert to authenticated
  with check (
    topic = 'machines:' || coalesce(auth.jwt() -> 'user_metadata' ->> 'machine_id', '')
    or topic in (
      select 'machines:' || m.id::text
      from machines m
      where m.user_id = auth.uid()
    )
  );
