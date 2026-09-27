# Teleboxel wire protocol (version 1)

What a client needs to talk to the server. The code is the reference:
`src/protocol.rs` (bytes), `src/main.rs` (connection), `src/world.rs` (sync).

## Connection

- WebSocket at `ws://<host>:3000/`. Binary messages only; a text message is a
  protocol error.
- The server answers pings and close frames. It sends no pings.

## Units and coordinates

- 1 voxel = 1 unit. **Y is up.** Voxel coordinates are `i32`.
- A voxel is a `u16` block id. `0` is air; every other meaning is the game's.
- Chunks are 16×16×16 voxels. Chunk coordinate = `voxel >> 4` (arithmetic
  shift: floors negatives). Local coordinate = `voxel & 15`.
- Index inside a chunk: `x | z << 4 | y << 8` (local coordinates), so each
  horizontal layer is contiguous.
- Entity positions are `i32` fixed point, 1/256 voxel (24.8): ±2^23 voxels per
  axis. Entity chunk = `pos >> 12`.
- The world is sparse: only chunks that were ever edited exist and are sent.
  A chunk absent from the server is all air. A chunk you have not received
  is unknown: it may exist outside your view or still be pending.

## Framing

- All integers are little-endian, fixed width, unaligned, no padding.
- Client → server message: messages, no header. After `HELLO` it may hold
  none (a no-op).
- Server → client message: `u32 tick`, then one or more messages.
- Each message starts with a `u8 type`. There is no per-message length: the
  size follows from the contents. Apply messages in order.
- `tick` counts world steps at `tick_hz`, for interpolating by server tick
  rather than by arrival time. It is a `u32` that starts at 0 and counts
  up by one per step. The server sends no
  message when it has nothing for you, so ticks can be skipped. The frame
  after `WELCOME` can carry the same tick.

## Client → server

| Type   | Name           | Body                                                    | Size         |
| ------ | -------------- | ------------------------------------------------------- | ------------ |
| `0x01` | `HELLO`        | `u16 protocol_version (1), u8 view_h, u8 view_v`        | 5            |
| `0x02` | `ENTITY_STATE` | `i32 x, i32 y, i32 z (24.8), u8 len, len × u8 data`     | 14 + len     |
| `0x03` | `VOXEL_EDITS`  | `u16 count, count × { i32 x, i32 y, i32 z, u16 block }` | 3 + 14·count |

- `HELLO` must be the whole first message, and is only valid there.
- `ENTITY_STATE` creates your entity the first time, and updates it after.
  `data` is opaque to the server and forwarded as is.
- `VOXEL_EDITS` writes its voxels in order.

## Server → client

| Type   | Name            | Body                                                                  | Size         |
| ------ | --------------- | --------------------------------------------------------------------- | ------------ |
| `0x81` | `WELCOME`       | `u32 entity_id, u8 tick_hz`                                           | 6            |
| `0x82` | `ENTITY_STATE`  | `u32 id, i32 x, i32 y, i32 z (24.8), u8 len, len × u8 data`           | 18 + len     |
| `0x83` | `ENTITY_REMOVE` | `u32 id`                                                              | 5            |
| `0x84` | `CHUNK`         | `i32 cx, i32 cy, i32 cz, 4096 × u16 block` (index order)              | 8205         |
| `0x85` | `CHUNK_EDITS`   | `i32 cx, i32 cy, i32 cz, u16 count, count × { u16 index, u16 block }` | 15 + 4·count |
| `0x86` | `CHUNK_UNLOAD`  | `i32 cx, i32 cy, i32 cz`                                              | 13           |

- `WELCOME` is the first server message, alone in its frame. `entity_id` is
  the id other clients see for your entity. Ids are never reused while the
  server runs.
- `ENTITY_STATE` adds an entity you don't have, or replaces it.
- `ENTITY_REMOVE`, `CHUNK_EDITS` and `CHUNK_UNLOAD` only refer to entities
  and chunks you have.
- `CHUNK` adds a chunk, or replaces one you have.
- `CHUNK_UNLOAD`: forget the chunk (treat it as air).
- A frame carries at most one message per entity and per chunk.

## Lifecycle

1. Connect and send `HELLO`. You can send more messages right away. You
   never have to wait for `WELCOME`.
2. `WELCOME` arrives at the next tick.
3. Until your first `ENTITY_STATE` you have no view and receive nothing
   else.
4. Your commands take effect at the next tick. Their results arrive in that
   tick's frame. If you send several `ENTITY_STATE` in one tick, others see
   the last one. Sending one, even unchanged, sends it to everyone who sees
   you. Send it only when it changes.
5. Frames arrive in order, and none is dropped. The server keeps up to 32
   frames per client that are not written to the socket yet. When a new frame
   finds that queue full (under sustained output, about 1 s of not reading),
   it drops the connection without a close frame.
6. On a protocol error the server closes the connection (see below).

### What you receive

Your view is centered on the chunk `c` that contains your entity. A chunk or
entity at chunk `p` is **in view** when `|p.x − c.x| ≤ H`, `|p.z − c.z| ≤ H`
and `|p.y − c.y| ≤ V`, where `H` and `V` are your `view_h` and `view_v`.
Something you have stays while it is within `H + 1` / `V + 1`
(hysteresis).

- **Entities**, checked every tick:
  - you get `ENTITY_STATE` when an entity enters your view, and whenever it
    changes while you have it;
  - you get `ENTITY_REMOVE` when it goes beyond `H + 1` / `V + 1`, or
    disconnects.
  - You never receive your own entity.
- **Chunks**, when your center chunk changes:
  - chunks you have beyond `H + 1` / `V + 1` get `CHUNK_UNLOAD`;
  - existing chunks in view that you don't have arrive as `CHUNK`, nearest
    first. At most 8 of these arrive per frame, and only while the server has
    no earlier frame queued for you, so entering a built area fills in over
    several ticks. The limit is only for this view fill: edits (below) can
    put more `CHUNK`s in one frame.
- **Edits**, from every client, including you:
  - a chunk you have gets `CHUNK_EDITS` wherever it is, even outside your
    view, with that tick's edits in the order the server applied them;
  - above 2048 edits in one tick, you get the whole `CHUNK` instead;
  - a chunk you don't have gets `CHUNK` if it is in view, and nothing
    otherwise.
- The server's order is final: when two edits hit the same voxel, the one it
  applied last wins. Applying your own edits before the echo is optional.
  The echo corrects them.

## Close codes

| Code   | When                                                                |
| ------ | ------------------------------------------------------------------- |
| `1002` | Malformed input; nothing of that message is applied. Reasons below. |
| `1011` | `entity ids exhausted`, sent instead of `WELCOME`.                  |

The `1002` reasons:
- `first message must be HELLO`
- `unsupported protocol version`
- `view_h too large`
- `view_v too large`
- `HELLO must be the whole message`
- `truncated message`
- `unexpected message type`
- `text messages are not supported`
- for an invalid WebSocket frame, a reason from the WebSocket library (e.g.
  `Frame too large`). A close frame with an invalid code gets `1002` with
  your own reason bytes.

Your own valid close is echoed.

## Limits

- `view_h ≤ 16`, `view_v ≤ 8` (chunks).
- Entity `data` ≤ 255 bytes.
- Edits per `VOXEL_EDITS` ≤ 65535. Send more messages for more edits.
- Each incoming WebSocket frame's payload < 64 MiB (a limit per frame, not
  per reassembled message); batching many messages can reach it.
- `CHUNK_EDITS.count` ≤ 2048.
