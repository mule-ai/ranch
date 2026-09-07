# Ranch Wire Protocol — v0 (draft)

One frame schema for every transport: the local unix socket and the
Supabase Realtime relay. All frames are **JSON objects** ("frames").

- **Local transport**: one frame per line (JSON-lines) over
  `~/.local/state/ranch/daemon.sock`.
- **Relay transport**: each frame is the `payload` of a Supabase
  Realtime broadcast message on the private channel
  `ranch:<machine_id>`:
  `{"topic":"ranch:<machine_id>","event":"broadcast","payload":<frame>}`.

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

- **`snapshot`** — full pane state. Chunked (§5). Final chunk carries
  `"end": true`. Contents:
  ```jsonc
  {
    "cols": 200, "rows": 50,
    "grid": [[cell, cell, ...], ...],      // row-major, all `rows` rows
    "cursor": {"x": 12, "y": 7, "hidden": false},
    "scrollback_top": 8123,                // offset of first line in `history`
    "history": [[cell,...], ...],          // tail of scrollback (≤ ~200 lines)
    "title": "work",
    "meta": {"kind": "shell"} | {"kind":"forge","ref":"<uuid>","status":"running"}
  }
  ```
  A **cell** is compactly encoded:
  `[text, style-id]` where `style-id` indexes a per-snapshot style
  table (consecutive same-style cells are merged into one cell):
  ```jsonc
  "styles": [ {"fg": "#d8dee9", "bg": null, "bold": true, "ul": false} ]
  ```
  `text` is the codepoint string for that run (may be multi-codepoint;
  wide chars count as 2 cells — renderer must measure, or the daemon
  may emit padded runs; MVP: daemon pads wide-char runs with U+00A0
  continuations so runs are cell-aligned).
- **`update`** — `{"seq": n, "rows": [ [cell,...] ... ]  // absolute row
  indices in a sibling "rows" map keyed by y, "cursor": {...},
  "title"?: "..."}`. Semantics: *these rows are now exactly this.*
  Coalesced per tick (~30 ms). Droppable: any newer `update` with a
  higher `seq` that covers the same rows supersedes it.
- **`scrollback`** — paged history: `{"offset": 0, "lines": [[cell,...]]}`
  in response to `scrollback-req`.
- **`meta`** — status updates without screen change:
  `{"kind":"forge","status":"running"|"done"|"error","preview":"last 240 chars"}`.

### Control (both directions)

- **`sessions.create`** `{name?, kind?:"shell"|"forge"|"mule", ref_id?,
  cols, rows}` → `sessions.ack {session, pane}`
- **`sessions.rename`** `{session, name}`
- **`sessions.kill`** `{session}` (kills all panes in it)
- **`sessions.pane-split`** `{session, pane, "h"|"v"}` → new pane id
- **`sessions.pane-kill`** `{session, pane}`
- **`sessions.select`** `{session, pane}` — sets the session's active
  pane (used when attaching without a pane, and for UI)
- **`mule.run`** `{workflow_id, params?}` → spawns/streams a workflow
  pane (§SPEC 8)
- **`error`** — `{"of": "<frame id>", "message": "..."}` for any failed
  request.

### Sync

- **`resync`** — client → daemon, sent when a `seq` gap is detected on
  an attached pane: daemon re-sends `snapshot`.
- The daemon also re-snapshots automatically on: relay reconnect,
  canonical size change, and when a client attaches.

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
