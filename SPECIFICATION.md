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
- The server stores a chunk as `[u16; 4096]` (8 KiB) plus a `u32 version`.
  The version starts at 0 (air) and goes up by 1 at the end of each tick in
  which the chunk was edited.

### 3.2 Entities

An entity is the generic "thing a client moves that others see". The server
does not know if it is a player, a car or a cursor.

```
Entity { id: u32, owner: client, pos: [i32; 3], data: bytes (≤255), version: u32 }
```

- The server **interprets only `pos`**. It needs it to decide who is near whom.
- `data` is opaque to the server: orientation, animation, name, color... It is
  forwarded as is.
- v0: one entity per connection, owned by that connection. It is created when
  the client first sends `ENTITY_STATE`, and destroyed on disconnect.
- `version` goes up by 1 for each `ENTITY_STATE` received.

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

This is the core idea of the server.

### 5.1 State-based sync

The server keeps, for each client, **what that client currently has**:

- `known_chunks: HashMap<ChunkPos, u32>`: the chunk version the client has.
  A chunk that is not in the map counts as version 0 (air).
- `known_entities: HashMap<EntityId, u32>`: the entity version the client has.

On each tick the server compares the truth with the client's view and sends
the difference. Whatever was not delivered is still a difference on the next
tick, so nothing is lost and the protocol needs no acks or resend requests.

### 5.2 Commit on enqueue

For each client, the tick builds **one frame** plus a list of pending updates
to that client's known-state.

- `try_send(frame)` returns `Ok`: apply the pending updates. The frame is
  queued, and TCP delivers it in order.
- It returns `Full`: discard the frame and the pending updates. The same
  difference (updated) is sent on a later tick.
- It returns `Closed`: the client is gone and gets removed.

The world task never awaits a client. A slow client only falls behind.

### 5.3 What goes in a client's frame

Chunks, for each chunk that exists and is in view, with `k` = known version and
`v` = current version:

| Condition                                        | Send                              |
| ------------------------------------------------ | --------------------------------- |
| `k == v`                                         | nothing                           |
| `k == v−1`, edited this tick, ≤ 2048 edits       | `CHUNK_EDITS` (this tick's edits) |
| otherwise                                        | `CHUNK` (snapshot)                |

- The 2048-edit limit keeps `count` inside a `u16` and makes sure a delta is
  never larger than a snapshot (2048 × 4 B = 8 KiB).
- A chunk created this tick has `k = 0`, `v = 1`, so it arrives as a small
  `CHUNK_EDITS`, not as an 8 KiB snapshot.
- Chunks in `known_chunks` that left the view (with hysteresis) get
  `CHUNK_UNLOAD`.
- Snapshots go **nearest first** (squared distance in chunks), at most
  `MAX_SNAPSHOTS_PER_TICK` per frame, and **only when the client's outbound
  queue is empty**. The rest waits for later ticks.
  *Need:* entering a built area can mean megabytes of snapshots. Without the
  cap, they would all go in one frame. Without the queue check, a slow link
  would fill the queue with snapshots and entity updates would wait behind
  them; an empty queue means the writer is keeping up. Without the ordering,
  far chunks could arrive before near ones.

Entities, for each entity (not the client's own) whose chunk is in view:

- If it is not known, or its known version differs from the current one:
  `ENTITY_STATE`.
- Known entities that left the view (with hysteresis) or were destroyed get
  `ENTITY_REMOVE`.

A frame carries at most one message per chunk and one per entity. If there is
nothing to send, no frame is sent.

### 5.4 Transport boundary

The world produces `Bytes` frames and knows nothing about WebSocket. State-based
sync already tolerates dropped frames, so a future unreliable transport would
not change the world.

## 6. Tick

Fixed rate `TICK_HZ`. The world task:

1. Drains the inbound queue, applying commands in arrival order:
   - `ENTITY_STATE`: overwrite pos/data, `version += 1`. Create the entity if
     needed.
   - `VOXEL_EDITS`: write the voxels (creating chunks as needed) and append
     `(index, block)` to that chunk's edit list for this tick.
2. `version += 1` for each edited chunk.
3. For each client: build the frame (§5.3), `try_send`, commit or discard (§5.2).
4. Clear the per-tick edit lists.

Inbound commands are only read at the tick. Nothing is sent between ticks, so
reading them earlier would only add wakeups.

Commands are validated once, in the connection layer (§8). The world trusts
them and does not re-check.

Per-client work is bounded by what changed, not by the view size:

- **When the client's center chunk changes**: scan the view box once. Existing
  chunks the client does not have become its `pending` list, sorted by
  distance. Known chunks outside the hysteresis box get `CHUNK_UNLOAD`.
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
- Versions never go over the wire. The server alone decides snapshot vs delta.

## 8. Connection lifecycle

1. WebSocket upgrade.
2. Read `HELLO` and validate it (§7.2).
3. Allocate `entity_id` from a process-wide atomic counter (never reused while
   the server runs). Send `WELCOME`. Send `Join { id, tx }` to the world.
   Nothing waits for the world: the id is known locally, and the world learns
   about the client at its next tick.
4. Active: a reader parses and validates messages and forwards commands to
   the world. A writer forwards the world's frames to the socket.
5. On close, error or protocol error: send `Leave { id }` to the world, which
   removes the client and its entity. Structure the reader as an inner
   function that returns `Result`, and send `Leave` after it, whatever it
   returned. No drop guard is needed.

Rules

- Reader and writer are separate tasks, or halves of a split socket.
  *Need:* `fastwebsockets::read_frame` is not cancel-safe, so it cannot sit in a
  `select!` next to the outbound channel.
- Malformed input never panics.

## 9. Constants (v0 defaults)

| Name              | Value                  | Why                                              |
| ----------------- | ---------------------- | ------------------------------------------------ |
| `TICK_HZ`         | 30                     | Standard rate; clients interpolate               |
| `MAX_VIEW_H`      | 16 chunks              | Bounds per-client tick cost                      |
| `MAX_VIEW_V`      | 8 chunks               | Same                                             |
| `MAX_SNAPSHOTS_PER_TICK` | 8 per client    | 64 KiB max per frame; ≈ 2 MiB/s at 30 Hz         |
| `OUTBOUND_QUEUE`  | 4 frames               | Small queue = low latency; state sync covers drops |
| `INBOUND_QUEUE`   | 1024 commands          | Shared connection → world queue                  |

`PROTOCOL_VERSION` = 1.

## 10. Code layout

- `src/main.rs`: HTTP/WebSocket accept, connection reader/writer.
- `src/protocol.rs`: type constants, client→server decoders, server→client
  encoders.
- `src/world.rs`: world state, tick, per-client sync.

World state:

```
World  { clients: HashMap<u32, Client>, chunks: HashMap<ChunkPos, Chunk>, tick: u32 }
Client { tx, view_h, view_v, entity: Option<Entity>, known_chunks, known_entities, pending }
Chunk  { blocks: [u16; 4096], version: u32, edits: Vec<(u16, u16)> }
```

In v0 a client and its entity are one record: the entity id is the client id.
There is no separate entity map. Splitting them is the first step when
several entities per client are needed (§13).

Dependencies stay: `tokio`, `axum`, `fastwebsockets`, `bytes`.

## 11. Performance goals and measurement

Targets at 50 clients:

- Tick work ≤ 1 ms.
- The world task never awaits a client.
- Latency added by the server ≤ 1 tick.

Built-in stats, printed every few seconds: tick time avg/max, bytes out,
frames sent/dropped, clients, entities, chunks.

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
   `ENTITY_STATE`/`ENTITY_REMOVE` with commit-on-enqueue.
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
2. Sharing encoded snapshots between clients (`Bytes` cache per chunk version).
3. Zero-allocation frame building (reused `BytesMut` split/freeze).
4. A faster transport (WebTransport/UDP).

Features:

- `PING`/`PONG` for client-side RTT.
- Several entities per client, entities owned by the server.
- Input-based authority, prediction and reconciliation.
- Validation, rate limits, auth.
- Persistence.
