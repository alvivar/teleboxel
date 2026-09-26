# Teleboxel Specification (v0)

This document is the source of truth. `docs/protocol-draft.txt` is historical
reference only; where they differ, this document wins.

## 1. Purpose

Teleboxel is a **minimal, fast core** for a shared voxel world:

> The server owns a **sparse, infinite voxel grid** and a set of **replicated
> entities**. Clients edit the grid and move their entities. The server orders,
> stores and forwards changes to whoever is nearby.

It is a learning project. The goal is the smallest correct implementation that
is fast. Performance is measured, not assumed.

## 2. Goals and non-goals

Goals

- Correct replication of voxels and entities to nearby clients.
- Minimal code and minimal protocol: only what the core needs.
- A world tick that never blocks on a client.
- Measurable performance (see §11).
- Any client type: browser (JS), native (Rust, Godot, Unity...).

Non-goals (v0)

- Security, auth, anti-cheat, rate limiting. Clients are trusted.
- Persistence. The world lives in memory.
- Physics or gameplay rules. They live in the client.
- Scale beyond ~50 concurrent clients.
- Transports other than WebSocket. The design keeps this swappable (§5.4).

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
Entity { id: u32, owner: client, pos: [i32; 3], data: [u8; ≤64], version: u32 }
```

- The server **interprets only `pos`**. It needs it to decide who is near whom.
- `data` is opaque to the server: orientation, animation, name, color... It is
  forwarded as is.
- v0: one entity per connection, owned by that connection. It is created when
  the client first sends `ENTITY_STATE`, and destroyed on disconnect.
- `version` goes up by 1 for each `ENTITY_STATE` received.

### 3.3 Clients and views

Each client has a **view**: an axis-aligned box of chunks centered on the chunk
that contains its entity.

- Chunk `p` is **in view** if `|p.x−c.x| ≤ H`, `|p.z−c.z| ≤ H` and
  `|p.y−c.y| ≤ V`, where `c` is the center chunk. `H` and `V` are the
  horizontal and vertical radii.
- Something already sent stays sent while it is within `H+1` / `V+1`
  (hysteresis). This avoids thrashing at the border.
- A client with no entity yet has no view and receives nothing but
  `WELCOME`/`PONG`.
- A client never receives its own entity.

## 4. Coordinates and units

- 1 voxel = 1 world unit. **Y is up.**
- Voxel coordinates: `i32 x, y, z`, valid range `[−2^23, 2^23)` on each axis.
- Chunk coordinate = `voxel >> 4` (arithmetic shift, floors negatives).
  Local coordinate = `voxel & 15`.
- Local voxel index inside a chunk: `i = x | (z << 4) | (y << 8)` (Y-major, so
  each horizontal layer is contiguous).
- Entity position: `i32` fixed point, **1/256 voxel** (24.8). Its range matches
  the voxel range exactly. Entity chunk = `pos >> 12`.

## 5. Synchronization model

This is the core idea of the server.

### 5.1 State-based sync, not message-based

The server keeps, for each client, **what that client currently has**:

- `known_chunks: HashMap<ChunkPos, u32>`: the chunk version the client has.
  A chunk that is not in the map counts as version 0 (air).
- `known_entities: HashMap<EntityId, u32>`: the entity version the client has.

On each tick the server compares the truth with the client's view and sends
the difference. Nothing is ever "lost": whatever was not delivered is still a
difference on the next tick.

### 5.2 Commit on enqueue

For each client, the tick builds **one frame** plus a list of pending updates
to the client's known-state.

- `try_send(frame)` returns `Ok`: apply the pending updates. The frame is
  queued, and TCP delivers it in order.
- It returns `Full`: discard the frame and the pending updates. The same
  difference (updated) is sent on a later tick.
- It returns `Closed`: the client is gone and gets removed.

The world task never awaits a client. A slow client only falls behind.

### 5.3 What goes in a client's frame

Chunks, for each chunk that exists and is in view, with `k` = known version and
`v` = current version:

| Condition                                  | Send                   |
| ------------------------------------------ | ---------------------- |
| `k == v`                                   | nothing                |
| `k == v−1` and the chunk was edited this tick | `CHUNK_EDITS` (this tick's edits) |
| otherwise                                  | `CHUNK` (snapshot)     |

- Chunks in `known_chunks` that left the view (with hysteresis) get
  `CHUNK_UNLOAD`.
- If a `CHUNK_EDITS` would be larger than the snapshot, send `CHUNK` instead.
- Snapshots are limited by a **per-client byte budget per tick**. Nearest
  chunks go first (squared distance in chunks). The rest waits for later ticks.

Entities, for each entity (not the client's own) whose chunk is in view:

- If it is not known, or its known version differs from the current one:
  `ENTITY_STATE`.
- Known entities that left the view (with hysteresis) or were destroyed get
  `ENTITY_REMOVE`.

A frame carries at most one message per chunk and one per entity. If there is
nothing to send, no frame is sent.

### 5.4 Transport independence

The world produces `Bytes` frames and knows nothing about WebSocket. The
connection layer only moves bytes. Replacing WebSocket later (WebTransport,
UDP) must not touch the world. State-based sync (§5.1) already tolerates
dropped frames, which is what an unreliable transport needs.

## 6. Tick

Fixed rate `TICK_HZ`. The world task:

1. Applies inbound commands as they arrive, in arrival order (between ticks and
   at the tick):
   - `ENTITY_STATE`: overwrite pos/data, `version += 1`. Create the entity if
     needed.
   - `VOXEL_EDITS`: write the voxels (creating chunks as needed) and append
     `(index, block)` to that chunk's edit list for this tick.
   - `PING`: remember the latest `client_time` for that client.
2. At the tick: for each edited chunk, `version += 1`.
3. For each client: build the frame (§5.3), `try_send`, commit or discard (§5.2).
4. Clear the per-tick edit lists.

Implementation notes (needed to stay fast, still little code):

- Recompute a client's candidate chunk list (existing chunks in view, sorted
  by distance) **only when its center chunk changes**. Do not scan the view
  box every tick.
- Each tick, check only the candidates the client does not have yet, plus the
  chunks edited this tick.
- Store entity `data` inline (`[u8; 64]` + len), not in a `Vec`.

## 7. Wire protocol

All integers are **little-endian**, fixed width, unaligned, no padding.
A WebSocket **binary message** is the unit of transport. Text messages are a
protocol error.

### 7.1 Framing

- Server → client message: `u32 tick`, then messages until the end of the buffer.
- Client → server message: messages until the end of the buffer (no header).
- Each message starts with a `u8 type`. There is no per-message length:
  every message's size follows from its contents.
- Messages are applied in order.
- An unknown type, a truncated message or an invalid value is a **protocol
  error**: the server closes the connection. The version check in
  `HELLO`/`WELCOME` is what protects compatibility.

### 7.2 Client → server

| Type   | Name           | Body                                                        |
| ------ | -------------- | ----------------------------------------------------------- |
| `0x01` | `HELLO`        | `u16 protocol_version, u8 view_h, u8 view_v`                |
| `0x02` | `ENTITY_STATE` | `i32 x, i32 y, i32 z, u8 len, [u8; len] data` (`len ≤ 64`)  |
| `0x03` | `VOXEL_EDITS`  | `u16 count (≥1), count × { i32 x, i32 y, i32 z, u16 block }` |
| `0x04` | `PING`         | `u32 client_time` (opaque to the server)                    |

- `HELLO` must be the first and only message of the first client WebSocket
  message.
- The server clamps `view_h`/`view_v` to `[1, MAX_VIEW_H]` / `[1, MAX_VIEW_V]`.
- The client applies its own edits optimistically. The server re-sends them to
  everyone, including the author. The server's order is final: when two edits
  hit the same voxel, the one the server applied last wins.

### 7.3 Server → client

| Type   | Name            | Body                                                            |
| ------ | --------------- | --------------------------------------------------------------- |
| `0x81` | `WELCOME`       | `u16 protocol_version, u32 entity_id, u8 tick_hz, u8 view_h, u8 view_v` |
| `0x82` | `ENTITY_STATE`  | `u32 id, i32 x, i32 y, i32 z, u8 len, [u8; len] data`           |
| `0x83` | `ENTITY_REMOVE` | `u32 id`                                                        |
| `0x84` | `CHUNK`         | `i32 cx, i32 cy, i32 cz, u8 encoding, payload`                  |
| `0x85` | `CHUNK_EDITS`   | `i32 cx, i32 cy, i32 cz, u16 count, count × { u16 index, u16 block }` |
| `0x86` | `CHUNK_UNLOAD`  | `i32 cx, i32 cy, i32 cz`                                        |
| `0x87` | `PONG`          | `u32 client_time` (echo of the latest `PING`)                   |

`CHUNK` encodings:

- `0` UNIFORM: `u16 block`. The whole chunk is one block (air included).
- `1` DENSE: `4096 × u16` in index order (§4). 8 KiB.

Notes

- `WELCOME` is the first server message. `entity_id` is the id that other
  clients will see for this client's entity. `view_h`/`view_v` are the values
  after clamping.
- `CHUNK_UNLOAD` means "forget this chunk" (treat it as air). It is only sent
  for chunks the client has.
- `PONG` rides in the next tick frame. The RTT it measures includes the
  server's tick wait, which is the real end-to-end latency.
- Versions never go over the wire. The server alone decides snapshot vs delta.

## 8. Connection lifecycle

1. WebSocket upgrade.
2. Wait for `HELLO` (timeout `HELLO_TIMEOUT`). On a wrong version, close.
3. Register with the world. The world allocates `entity_id` (never reused
   while the server runs). Send `WELCOME`.
4. Active: a reader parses and validates messages and forwards commands to
   the world. A writer forwards the world's frames to the socket.
5. On close, error or protocol error: the world removes the client and its
   entity. This must happen on **every** exit path (use a drop guard).

Rules

- Reader and writer are separate tasks, or halves of a split socket.
  `fastwebsockets::read_frame` is **not cancel-safe** and must not sit in a
  `select!` next to other branches.
- Parsing happens in the connection task, not in the world.
- Malformed input never panics.

## 9. Constants (v0 defaults)

| Name                  | Value      | Why                                          |
| --------------------- | ---------- | -------------------------------------------- |
| `PROTOCOL_VERSION`    | 1          |                                              |
| `TICK_HZ`             | 30         | Standard rate; clients interpolate           |
| `MAX_VIEW_H`          | 16 chunks  |                                              |
| `MAX_VIEW_V`          | 8 chunks   | Height is cheap in a sparse world            |
| `VIEW_HYSTERESIS`     | 1 chunk    |                                              |
| `MAX_ENTITY_DATA`     | 64 bytes   |                                              |
| `CHUNK_BUDGET`        | 64 KiB / client / tick | ≈ 2 MiB/s at 30 Hz               |
| `OUTBOUND_QUEUE`      | 4 frames   | Small queue = low latency; state sync covers drops |
| `INBOUND_QUEUE`       | 1024 cmds  | Shared connection → world queue              |
| `MAX_CLIENT_MESSAGE`  | 1 MiB      | Fits a full `VOXEL_EDITS` (65535 × 14 B)     |
| `HELLO_TIMEOUT`       | 5 s        |                                              |

## 10. Code layout

- `src/main.rs`: HTTP/WebSocket accept, connection reader/writer, drop guard.
- `src/protocol.rs`: type constants, encoders, bounds-checked decoders, tests.
- `src/world.rs`: world state, tick, per-client sync.

Dependencies stay: `tokio`, `axum`, `fastwebsockets`, `bytes`.

## 11. Performance goals and measurement

Targets at 50 clients:

- Tick work ≤ 1 ms (measured every tick; average and max logged).
- The world task never awaits a client.
- No per-entity or per-voxel allocations in the tick. At most one buffer per
  client frame.
- Latency added by the server ≤ 1 tick.

Built-in stats, printed every few seconds: tick time avg/max, bytes out,
frames sent/dropped, clients, entities, chunks.

Where the cost is expected to be: entity traffic is small (≈ 50 × ~80 B per
tick). Chunk traffic dominates (8 KiB per dense chunk). Optimization effort
goes to chunks first.

## 12. Implementation steps

Each step is small and testable, and leaves the server runnable.

1. **Protocol**: `protocol.rs` with encoders and decoders for all messages.
   Round-trip tests and malformed-input tests (truncated input, bad type, bad
   len, count 0).
2. **Connection**: split reader/writer, `HELLO`/`WELCOME`, drop guard, close on
   protocol error. Remove the text protocol.
3. **Entities**: inbound `ENTITY_STATE`, views, outbound
   `ENTITY_STATE`/`ENTITY_REMOVE` with commit-on-enqueue.
4. **Chunks**: sparse storage, `VOXEL_EDITS`, `CHUNK`/`CHUNK_EDITS`/`CHUNK_UNLOAD`,
   snapshot budget.
5. **Measurement**: `PING`/`PONG`, tick stats.
6. **Clients**: update `tools/client.html` (connect, move, edit, show counts),
   plus a bot binary that simulates N clients for load tests.
7. **Measure, then optimize** (§13).

## 13. Deferred (not v0)

Optimization experiments, in the expected order of payoff:

1. Better chunk encodings: RLE and/or a per-chunk palette with bit-packing.
2. Sharing encoded snapshots between clients (`Bytes` cache per chunk version).
3. Zero-allocation frame building (reused `BytesMut` split/freeze).
4. A faster transport (WebTransport/UDP). §5.4 keeps the world unchanged.

Features:

- Several entities per client, entities owned by the server.
- Input-based authority, prediction and reconciliation.
- Validation, rate limits, auth.
- Persistence.
