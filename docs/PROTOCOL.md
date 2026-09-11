# Ranch Wire Protocol — v0 (draft)

One frame schema for every transport: the local unix socket and the
Supabase Realtime relay. All frames are **JSON objects** ("frames").

- **Local transport**: one frame per line (JSON-lines) over
  `~/.local/state/ranch/daemon.sock`.
- **Relay transport**: each frame is the inner `payload` of a Supabase
  Realtime broadcast message on the private channel
  `realtime:machines:<machine_id>`:
  `{"topic":"realtime:machines:<id>","event":"broadcast",
    "payload":{"event":"frame","payload":<frame>}}`.
  The WS URL carries the anon key; the joining user's JWT (machine or
  owner) rides in the join payload as `access_token`. Realtime gates the
  channel via RLS on `realtime.messages` (migration 0003): only the
  machine itself and the owner may read/broadcast.

The relay is a dumb, ordered-per-sender pipe. No assumptions about
delivery guarantees beyond "usually gets there, in order, per sender."

## 1. Frame envelope

```jsonc
{
  "v": 0,                     // protocol version
  "id": "f_9f2c...",          // unique frame id (client or daemon generated)
  "from": { "kind": "daemon" | "client", "id": "<client-id or machine-id>" },
  "to": {                     // addressing; missing fields = wildcard
    "machine": "m_...",        // required
    "session": "s_..." | null,
    "pane":    "p_..." | null
  },
  "type": "<frame type>",     // see §3
  "seq": 4123,                 // present on ordered data frames (daemon→client)
  ...type-specific fields
}
```

Addressing rules:

- daemon → client frames: `to` identifies the attaching client (or a
  session for "any client of this session").
- client → daemon frames: `to.machine` is the target machine;
  `to.session`/`to.pane` select the target.
- control frames (`sessions.*`) may omit `session`/`pane`.

## 2. Identity & lifecycle

| step | frame | direction | notes |
|---|---|---|---|
| connect | `hello` | client → daemon | `{proto:0, client_name, caps:[...]}` |
| | `hello-ok` | daemon → client | `{machine, sessions:[session-meta]}` — the session list is piggybacked |
| attach | `attach` | client → daemon | `{session, pane}` (pane omitted → active pane) |
| | `snapshot` | daemon → client | chunked; see §5 |
| live | `update` | daemon → client | coalesced dirty rows, see §4 |
| input | `input` | client → daemon | `{data: <base64 bytes>}` — already-encoded terminal input (Kitty/SGR sequences) |
| resize | `resize` | client → daemon | `{cols, rows}` — client requests the canonical size change |
| detach | `detach` | client → daemon | polite; absence of `hb` is the impolite version |
| liveness | `hb` | both | every 15 s; 3 missed ⇒ considered gone |

`client-id` is chosen by the client at `hello` (random uuid, persisted
per app install for mobile so reconnects are stable).

## 3. Frame types

### Data (daemon → client, ordered by `seq`)

- **`snapshot`** — full pane state. Chunked (§5). Carries the session's
  **split tree** (`layout`) plus one pane-snapshot per pane:
  ```jsonc
  {
    "layout": {"k":"split","dir":1,"pct":48,                 // pct = space to `a`
               "a":{"k":"Leaf","pane":"<uuid>"},
               "b":{"k":"Leaf","pane":"<uuid>"}},
    "active_pane": "<uuid>",
    "panes": [ {"id":"<uuid>", "cols":48, "rows":29,
                 "lines":["…"], "cursor":{"x":12,"y":7,"visible":true},
                 "kind":"pty"|"forge-chat",
                 "chat":[…], "forge_session":"<uuid>"} ]
  }
  ```
  Each pane runs at its own (cols, rows) computed by the daemon from
  the tree; the client computes screen rectangles the same way and
  renders pane-by-pane. A single-pane session is `Leaf` and renders
  borderless full-screen.
  - **`update`** — `{"pane": "<uuid>", "cols": n, "rows": n, "seq": n,
  "rows_upd": [[row_index, text], ...], "cursor": {...}}`.
  Semantics: *these rows are now exactly this* (pane-local indices).
  Coalesced per tick (~30 ms). Droppable: any newer `update` with a
  higher `seq` that covers the same rows supersedes it.
- **`scrollback`** — paged history: `{"offset": 0, "lines": [[cell,...]]}`
  in response to `scrollback-req`.
- **`meta`** — status updates without screen change:
  `{"kind":"forge","status":"running"|"done"|"error","preview":"last 240 chars"}`.

### Control (both directions)

- **`sessions.create`** `{name?, kind?:"shell"|"forge", cwd?}` →
  `sessions.ack {session, pane}`. `kind:"forge"` is a first-class
  **agent session**: its panes run the `pi` agent (via a login shell so
  the user's toolchain PATH applies) with `cwd` as the working
  directory (default `$HOME`). The kind surfaces on
  `machines_info`/session lists so clients can badge agent sessions.
- **`sessions.rename`** `{session, name}`
- **`sessions.kill`** `{session}` (kills all panes in it)
- **`sessions.pane-split`** `{session, pane, dir}` — replaces `pane`'s
  leaf in the session's split tree with a split node (`dir`: `0` =
  top/bottom, `1` = left/right; the new pane is the `b` child).
  Daemon answers with `sessions.ack {session, pane: <new-id>}` and
  re-snapshots attached clients (sibling shrinks; every pane's PTY is
  `SIGWINCH`ed at its leaf size).
- **`sessions.pane-resize`** `{session, pane, dir, delta}` — adjusts the
  percent split of the node directly containing `pane` along `dir` by
  `delta` cells (positive grows `pane`'s side). Clamped so every leaf
  keeps ≥ 4 cells. Daemon re-snapshots on change.
- **`sessions.pane-kill`** `{session, pane}` — removes the leaf and
  promotes its sibling subtree.
- **`sessions.window-new`** `{session, name?}` — new window (one pane,
  auto-named by index); becomes active. Daemon re-snapshots.
- **`sessions.window-select`** `{session, window}` — activate a window
  by id. Its panes resize to the session geometry (`SIGWINCH`); the
  other window's panes keep their sizes.
- **`sessions.window-next`** `{session, delta}` — ring-walk the window
  stack (tmux `next/previous-window`).
- **`sessions.window-kill`** `{session, window}` — kill a window and
  its panes; killing the last window kills the session.
- **`sessions.window-rename`** `{session, window, name}`.
- **`sessions.pane-swap`** `{session, a, b}` — the two panes' rectangles
  trade places in the split tree; each pane keeps its own PTY/VT state
  (shell, running programs). Both PTYs are `SIGWINCH`ed to their new
  geometry and the daemon re-snapshots. Desktop binding: prefix `{` /
  `}` swaps the focused pane with the previous/next pane in layout
  traversal order (wraps).
- **`sessions.select`** `{session, pane}` — sets the session's active
  pane (used when attaching without a pane, and for UI)
- **`mule.run`** `{workflow_id, params?}` → spawns/streams a workflow
  pane (§SPEC 8)
- **`chat.send`** `{session, pane, text}` — send a user message to a
  **forge-chat pane**. The daemon POSTs to the forge API
  (`POST /messages`) on its worker thread.
- **`chat`** `{pane, msgs, reset?}` (daemon → client) — conversation
  rows for a forge-chat pane: appends in sequence order, or a full
  replacement when `reset` (snapshot semantics). Rows mirror forge
  `messages`: `{seq, role: "user"|"assistant"|"tool", text,
  tool_name?, tool_output?, duration_ms?, created_at?}`.
- **`error`** — `{"of": "<frame id>", "message": "..."}` for any failed
  request.

### Files (M10 — mobile editor)

The daemon is the file server; clients (mobile/CLI) are thin editors.

- **`DirList`** `{req_id, path?}` → **`DirListOk`** `{req_id, path,
  parent?, dirs[], files[]}` — list a directory (default `$HOME`).
  `dirs` = subdirectories, `files` = regular files; both sorted; hidden
  entries excluded (same filter as `dirs`).
- **`FileRead`** `{req_id, path}` → **`FileReadOk`** `{req_id, path,
  content, mtime, size}` — read a file as UTF-8. `mtime` is a unix
  timestamp; clients store it and pass it to `FileWrite` for conflict
  detection. Errors (no such file, not a regular file, unreadable, or
  size > `FILE_MAX_BYTES` (256 KiB)) → `error`.
- **`FileWrite`** `{req_id, path, content, mtime?}` → **`FileWriteOk`**
  `{req_id, path, mtime}` — atomically write the file (write to a temp
  file in the same dir, then `rename`). If `mtime` is supplied and the
  on-disk mtime no longer matches, the daemon replies `error` with
  `message` starting `file changed on disk` and does **not** clobber.
  A missing/nil `mtime` skips the check (first-save or client unaware).
  Parent directory must exist; creating files is in scope, creating new
  directories is not (MVP).
- **`FileChanged`** `{path, mtime}` — daemon → client push (M10 phase 3).
  The daemon auto-watches every file a client reads (`FileReadOk` seeds
  the watch, `FileWriteOk` refreshes its baseline so the client's own
  saves are quiet) and stats the watched set every ~2 s on its tick loop.
  A changed on-disk mtime is pushed to *that* client only; clients with
  the file open surface a conflict (reload vs keep local) instead of
  discovering the clobber risk only at save time.

### Sync

- **`resync`** — client → daemon, sent when a `seq` gap is detected on
  an attached pane: daemon re-sends `snapshot`.
- The daemon also re-snapshots automatically on: relay reconnect,
  canonical size change, and when a client attaches.

### Agent panes (M9)

- `sessions-create` gains `kind: "pi"` — the pane is a chat pane backed
  by a **local** `pi --mode rpc` child (spawned in `cwd`, default
  `$HOME`). Same UX as forge panes; `PaneSnap.kind` stays
  `"forge-chat"` with `forge_session` absent.
- `sessions-create` gains `forge_session: <uuid>` (with `kind:
  "forge"`) — **adopt** an existing forge session instead of creating
  one. The daemon watches its SSE stream; history replays into the
  pane (`since=0` catch-up), so resume is a no-op client-side.
- `forge-list {req_id}` → `forge-list-ok {req_id, sessions[]}` — the
  daemon proxies `GET /sessions` from the lab forge (worker thread;
  broadcast to all clients, matched by `req_id`). Each row:
  `{id, title, updated, ended?}`.
- Chat panes with `forge_session == nil` are local-pi backed; the
  daemon routes `chat-send` to the child's stdin as
  `{"type":"prompt","message":...}` and maps pi RPC events
  (`message_end`, `tool_execution_*`, `turn_end`) to `chat` rows and
  `meta kind="agent"` status.

## 4. Coalescing & sequencing

- The daemon maintains, per pane, a `seq` counter and a dirty-row set.
  On each tick (default 30 ms, configurable), if dirty rows exist it
  emits one `update` containing all dirty rows since the last tick.
- `seq` increments per emitted `update` (per pane). Clients track
  `last_seq` per pane; `update.seq != last_seq + 1` ⇒ `resync`.
- `update` and `snapshot` are the only ordered data frames; `meta` and
  control frames are out-of-band (idempotent, or ACK'd by `error`).

## 5. Chunking & size limits

Supabase Realtime caps broadcast payloads (~28 KB; verify at M2).
Local unix socket has no practical limit, but the same chunking is used
everywhere for uniformity.

- Any frame whose serialized form exceeds **16 KB** is split into
  `chunk` frames:
  ```jsonc
  { "type":"chunk", "chunk_id":"c_...", "i": 0, "n": 3, "data": "..." }
  ```
  Reassembly is by `(chunk_id, i, n)`; a torn batch (missing part)
  triggers `resync` if it was a `snapshot`/`update`.
- `snapshot` is always chunked (grid is ~cols·rows·~10 B typical).
- Default `update` frames are small; the 16 KB cap protects bursts.

## 6. Client-side invariants

1. Never assume `update` delivery; assume eventual state + `resync`.
2. Render from the style table; cache styles across a snapshot.
3. Send `resize` only on actual client size change; the daemon applies
   it as the canonical pane size (tmux rule: most-recent client wins).
4. On `hello-ok`/attach, wait for a complete `snapshot` before
   rendering; on mid-batch reconnect, discard partial batches.

## 7. Extensibility

- Unknown `type` ⇒ ignore (log at debug).
- `v` bump rules: additive fields = no bump; semantics change = bump +
  `caps` negotiation in `hello`.
- Post-MVP additions designed-for: structured agent frames
  (`agent.message`, `agent.tool`) for chat-style forge rendering —
  they travel as control frames alongside the VT pane, keyed by the
  same `to` address, so a client can switch between "raw TUI" and
  "chat view" of the same session without protocol changes.
