# Teleboxel Specification (v0)

This document is the source of truth. `docs/protocol-draft.txt` is historical
reference only; where they differ, this document wins.

## 1. Purpose

Teleboxel is a **minimal, fast core** for a shared voxel world:

> The server owns a **sparse, infinite voxel grid** and a set of **replicated
> entities**. Clients edit the grid and move their entities. The server orders,
> stores and forwards changes to whoever is nearby.

It is a learning project. The goal is the simplest correct implementation that
is fast. Performance is measured, not assumed. Every piece in this document
exists for a stated need. Anything else is deferred (§13).

## 2. Goals and non-goals

Goals

- Correct replication of voxels and entities to nearby clients.
- A world tick that never blocks on a client.
- Measurable performance (§11).
- Any client type: browser (JS), native (Rust, Godot, Unity...).

Non-goals (v0)

- Security, auth, anti-cheat, rate limiting. Clients are trusted.
- Persistence. The world lives in memory.
- Physics or gameplay rules. They live in the client.
- Scale beyond ~50 concurrent clients.
- Transports other than WebSocket.

## 3. Core model

### 3.1 Voxels and chunks

- A voxel is a single `u16` **block id**. `0` = air. All other meaning
  (material, rotation, variants) is encoded in the id by the game.
- The world is split into cubic chunks of `16×16×16` on all three axes.
  There is no height limit.
- The world is **sparse**: only chunks that have been edited exist. A missing
  chunk is all air.
- The server stores a chunk as `[u16; 4096]` (8 KiB).

### 3.2 Entities

An entity is the generic "thing a client moves that others see". The server
does not know if it is a player, a car or a cursor.

```
Entity { id: u32, pos: [i32; 3], data: bytes (≤255) }
```

- The server **interprets only `pos`**. It needs it to decide who is near whom.
- `data` is opaque to the server: orientation, animation, name, color... It is
  forwarded as is.
- v0: one entity per connection, owned by that connection. It is created when
  the client first sends `ENTITY_STATE`, and destroyed on disconnect.

### 3.3 Clients and views

Each client has a **view**: an axis-aligned box of chunks centered on the chunk
`c` that contains its entity.

- Chunk `p` is **in view** if `|p.x−c.x| ≤ H`, `|p.z−c.z| ≤ H` and
  `|p.y−c.y| ≤ V`, where `H` and `V` are the horizontal and vertical radii.
- Something already sent stays sent while it is within `H+1` / `V+1`
  (hysteresis). *Need:* without it, a client moving back and forth across a
  chunk border would make the server unload and resend a whole plane of chunks
  (up to 17×9 snapshots) every time.
- A client with no entity yet has no view and receives nothing but `WELCOME`.
- A client never receives its own entity.

## 4. Coordinates and units

- 1 voxel = 1 world unit. **Y is up.**
- Voxel coordinates: `i32 x, y, z`.
- Chunk coordinate = `voxel >> 4` (arithmetic shift, floors negatives).
  Local coordinate = `voxel & 15`.
- Local voxel index inside a chunk: `i = x | (z << 4) | (y << 8)` (Y-major, so
  each horizontal layer is contiguous).
- Entity position: `i32` fixed point, **1/256 voxel** (24.8), which gives a range
  of ±2^23 voxels per axis. Entity chunk = `pos >> 12`.

## 5. Synchronization model

### 5.1 The stream is reliable, or the client is gone

Every frame the world produces for a client is delivered, in order. There is
no drop path.

- `try_send(frame)` returns `Ok`: TCP delivers it.
- It returns `Full`: the client has not taken `OUTBOUND_QUEUE` frames (about a
  second). It is disconnected.
- It returns `Closed`: the connection already ended. It is removed the same
  way.

The world task never awaits a client. Because nothing is ever dropped, the
server only needs to remember **what each client has**, not which version:

- `known_chunks: HashSet<ChunkPos>`
- `known_entities: HashSet<EntityId>`
- `pending: Vec<ChunkPos>`: chunks in view the client does not have yet,
  nearest first.

### 5.2 What goes in a client's frame

Chunks:

- For each chunk **edited this tick**:
  - the client has it: `CHUNK_EDITS` with this tick's edits, wherever the chunk
    is. A known chunk can sit in the hysteresis ring, outside the view; if it
    got no edits there, the client's copy would stay stale when it came back
    into view, because a known chunk is never resent.
  - the client does not have it and it is in view: `CHUNK`, and add it to
    `known_chunks`.
  - If the edit list has more than 2048 entries, send `CHUNK` instead of
    `CHUNK_EDITS`: that keeps `count` inside a `u16` and a delta never larger
    than a snapshot.
- Then, **only when the client's outbound queue is empty**, up to
  `MAX_SNAPSHOTS_PER_TICK` chunks from the head of `pending` as `CHUNK`, adding
  each to `known_chunks`. Entries the client already got through an edit are
  skipped, not sent twice.
  *Need:* entering a built area can mean megabytes of snapshots. Without the
  cap, they would all go in one frame. Without the queue check, a slow link
  would fill the queue with snapshots and entity updates would wait behind
  them; an empty queue means the writer is keeping up. Nearest first, or far
  chunks could arrive before near ones.

Entities, for each entity except the client's own:

- Not known and in view: `ENTITY_STATE`, add to `known_entities`.
- Known and changed this tick: `ENTITY_STATE`.
- Known and outside the hysteresis box: `ENTITY_REMOVE`, remove from
  `known_entities`.
- Known and destroyed this tick: `ENTITY_REMOVE`, remove from `known_entities`.

A frame carries at most one message per chunk and one per entity. If there is
nothing to send, no frame is sent.

### 5.3 Transport boundary

The world produces `Bytes` frames and knows nothing about WebSocket. It does
assume an ordered, reliable stream. An unreliable transport would need
versioned state and a drop path; that is deferred (§13).

## 6. Tick

Fixed rate `TICK_HZ`. The world task:

1. Drains the inbound queue, applying commands in arrival order:
   - `Join`: send the client a frame with `WELCOME` and add it. If the send
     returns `Closed`, the connection already ended and its `Leave` follows,
     so the client is not added. The queue is new, so this send cannot be
     `Full`.
   - `Leave`: remove the client; record its entity id as destroyed this tick.
   - `ENTITY_STATE`: overwrite pos/data (creating the entity if needed) and
     record the entity as changed this tick.
   - `VOXEL_EDITS`: write the voxels (creating chunks as needed) and append
     `(index, block)` to that chunk's edit list for this tick.
2. For each client: build the frame (§5.2), `try_send`, disconnect on `Full`
   (§5.1).
3. Clear the per-tick records: chunk edit lists, changed entities, destroyed
   entities.

Inbound commands are only read at the tick. Nothing is sent between ticks, so
reading them earlier would only add wakeups.

Commands are validated once, in the connection layer (§8). The world trusts
them and does not re-check.

Per-client work is bounded by what changed, not by the view size:

- **When the client's center chunk changes**: scan the view box once. Existing
  chunks the client does not have become its `pending` list, sorted by
  distance. Known chunks outside the hysteresis box get `CHUNK_UNLOAD` and
  leave `known_chunks`.
- **Every tick**: the chunks edited this tick, the head of `pending`, and the
  entities. Entities are checked every tick because they move on their own.

*Need:* scanning the view box every tick is up to 33×33×17 lookups per
client. At 50 clients that alone exceeds the 1 ms target (§11). The center
chunk changes at most every 16 voxels of movement.

## 7. Wire protocol

All integers are **little-endian**, fixed width, unaligned, no padding.
A WebSocket **binary message** is the unit of transport.

### 7.1 Framing

- Server → client message: `u32 tick`, then messages until the end of the
  buffer. *Need:* clients interpolate entities by server tick, not by arrival
  time. Arrival time carries network jitter.
- Client → server message: messages until the end of the buffer (no header).
- Each message starts with a `u8 type`. There is no per-message length:
  every message's size follows from its contents.
- Messages are applied in order.
- Any malformed input is a **protocol error**. That covers a text message, an
  unknown type, a truncated message and an out-of-range value. The server closes
  the connection with code 1002 and a reason. It never skips or guesses.

### 7.2 Client → server

| Type   | Name           | Body                                                   |
| ------ | -------------- | ------------------------------------------------------ |
| `0x01` | `HELLO`        | `u16 protocol_version, u8 view_h, u8 view_v`           |
| `0x02` | `ENTITY_STATE` | `i32 x, i32 y, i32 z, u8 len, [u8; len] data`          |
| `0x03` | `VOXEL_EDITS`  | `u16 count, count × { i32 x, i32 y, i32 z, u16 block }` |

- `HELLO` must be the whole first client message.
- A version mismatch, `view_h > MAX_VIEW_H` or `view_v > MAX_VIEW_V` is a
  protocol error. *Need:* the view radius bounds the per-client tick cost.
- The client applies its own edits optimistically. The server re-sends them to
  everyone, including the author. The server's order is final: when two edits
  hit the same voxel, the one the server applied last wins.

### 7.3 Server → client

| Type   | Name            | Body                                                                  |
| ------ | --------------- | --------------------------------------------------------------------- |
| `0x81` | `WELCOME`       | `u32 entity_id, u8 tick_hz`                                           |
| `0x82` | `ENTITY_STATE`  | `u32 id, i32 x, i32 y, i32 z, u8 len, [u8; len] data`                 |
| `0x83` | `ENTITY_REMOVE` | `u32 id`                                                              |
| `0x84` | `CHUNK`         | `i32 cx, i32 cy, i32 cz, 4096 × u16 block` (index order, §4)          |
| `0x85` | `CHUNK_EDITS`   | `i32 cx, i32 cy, i32 cz, u16 count, count × { u16 index, u16 block }` |
| `0x86` | `CHUNK_UNLOAD`  | `i32 cx, i32 cy, i32 cz`                                              |

Notes

- `WELCOME` is the first server message. `entity_id` is the id that other
  clients will see for this client's entity.
- `CHUNK_UNLOAD` means "forget this chunk" (treat it as air). It is only sent
  for chunks the client has.

## 8. Connection lifecycle

1. WebSocket upgrade.
2. Read `HELLO` and validate it (§7.2).
3. Allocate `entity_id` from a process-wide atomic counter (never reused while
   the server runs). Send `Join { id, tx, disconnect, view_h, view_v }` to the
   world.
   Nothing waits for the world: the id is known locally, and the reader starts
   at once. The world sends `WELCOME` at its next tick (§6), so every server
   frame, the first included, carries a real tick. The connection never
   encodes server messages.
4. Active: a reader parses and validates messages and forwards commands to
   the world. A writer forwards the world's frames to the socket.
5. On close, error or protocol error: send `Leave { id }` to the world, which
   removes the client and its entity. Structure the reader as an inner
   function that returns `Result`, and send `Leave` after it, whatever it
   returned.
6. When the world removes a client (`Leave`, `Full`, `Closed`), it drops the
   client's `disconnect` sender (a `oneshot`). The connection waits for it next
   to the reader and next to the final close. *Need:* a client that stops
   reading blocks the writer mid-write, and with it the write half; only a
   signal from the world can end that connection.

Rules

- Reader and writer are separate tasks, or halves of a split socket.
  *Need:* `fastwebsockets::read_frame` is not cancel-safe, so it cannot sit in a
  `select!` next to the outbound channel. Cancelling it once, when the
  connection ends and never reads again, is fine.
- Malformed input never panics.

## 9. Constants (v0 defaults)

| Name                     | Value         | Why                                        |
| ------------------------ | ------------- | ------------------------------------------ |
| `TICK_HZ`                | 30            | Standard rate; clients interpolate         |
| `MAX_VIEW_H`             | 16 chunks     | Bounds per-client tick cost                |
| `MAX_VIEW_V`             | 8 chunks      | Same                                       |
| `MAX_SNAPSHOTS_PER_TICK` | 8 per client  | 64 KiB max per frame; ≈ 2 MiB/s at 30 Hz   |
| `OUTBOUND_QUEUE`         | 32 frames     | ≈ 1 s of stall before a client is dropped  |
| `INBOUND_QUEUE`          | 1024 commands | Shared connection → world queue            |

`PROTOCOL_VERSION` = 1.

## 10. Code layout

- `src/main.rs`: HTTP/WebSocket accept, connection reader/writer.
- `src/protocol.rs`: type constants, client→server decoders, server→client
  encoders.
- `src/world.rs`: world state, tick, per-client sync.

World state:

```
World  { clients: HashMap<u32, Client>, chunks: HashMap<ChunkPos, Chunk>, tick: u32,
         edited: Vec<ChunkPos>, changed: HashSet<u32>, destroyed: Vec<u32> }
Client { tx, disconnect, view_h, view_v, entity: Option<Entity>, view_center: Option<ChunkPos>,
         known_chunks, known_entities, pending }
Chunk  { blocks: [u16; 4096], edits: Vec<(u16, u16)> }
```

In v0 a client and its entity are one record: the entity id is the client id.
There is no separate entity map. Splitting them is the first step when
several entities per client are needed (§13).

Dependencies stay: `tokio`, `axum`, `fastwebsockets`, `bytes`.

## 11. Performance goals and measurement

Targets at 50 clients:

- Tick work ≤ 1 ms.
- The world task never awaits a client.

By construction, the server adds at most one tick of latency: a command
arrives, and its effect is in the next frame.

Built-in stats, printed every few seconds: tick time avg/max, bytes out,
clients, entities, chunks.

Where the cost is expected to be: entity traffic is small (≈ 50 × ~50 B per
tick). Chunk traffic dominates (8 KiB per snapshot). Optimization effort goes to
chunks first.

## 12. Implementation steps

Each step is small and leaves the server runnable.

1. **Protocol**: `protocol.rs`. The server only decodes client→server messages
   and only encodes server→client messages.
   Tests:
   - Decoders reject malformed input without panicking. *Risk:* the input is
     untrusted bytes.
   - Encoders produce exact bytes. *Risk:* clients in other languages depend on
     the layout.
2. **Connection**: split reader/writer, `HELLO`/`WELCOME`, `Join`/`Leave`,
   close on protocol error. Remove the text protocol.
3. **Entities**: inbound `ENTITY_STATE`, views, outbound
   `ENTITY_STATE`/`ENTITY_REMOVE`.
4. **Chunks**: sparse storage, `VOXEL_EDITS`, `CHUNK`/`CHUNK_EDITS`/`CHUNK_UNLOAD`,
   `pending` list and snapshot cap.
5. **Measurement**: tick stats (§11).
6. **Clients**:
   - Update `tools/client.html` to connect, move, edit and show counts.
   - Add a bot binary that simulates N clients. *Need:* the targets in §11 are
     defined at 50 clients.
7. **Measure, then optimize** (§13).

## 13. Deferred (not v0)

Optimization experiments, in the expected order of payoff:

1. Compact chunk encodings: UNIFORM (single-block chunk), RLE, a palette with
   bit-packing.
2. Sharing encoded snapshots between clients (`Bytes` cache per chunk).
3. Zero-allocation frame building (reused `BytesMut` split/freeze).

Features:

- Tolerating dropped frames (versioned state per client), which is what an
  unreliable transport (WebTransport/UDP) would need.
- `PING`/`PONG` for client-side RTT.
- Several entities per client, entities owned by the server.
- Input-based authority, prediction and reconciliation.
- Validation, rate limits, auth.
- Persistence.
