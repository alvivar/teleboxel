# Teleboxel Specification

## Purpose

Define the concrete, step-by-step implementation plan for a minimal, fast,
authoritative voxel + player synchronization server using WebSockets, based on
`docs/protocol-draft.txt` and the current server prototype in `src/main.rs`.

## Scope

- Server: authoritative world simulation, interest management, binary protocol.
- Client: minimal test client for validation and profiling.
- Protocol: v0 binary protocol with fixed-width fields (no varints, no RLE).

## Non-goals (v0)

- Persistence to disk
- Authentication or account systems
- Advanced physics
- Complex client rendering pipeline

## Protocol Decisions (v0)

These are concrete choices from the draft to remove ambiguity.

- Chunk size: 16x16x16.
- Position: chunk-relative `s16` centimeters + `i32` chunk coords.
- Orientation: yaw `u16`, pitch `i16` (no roll).
- Velocity: `i16` cm/s per axis (optional).
- Player state bits: `u16`.
- Voxel flags: `u8` with bit0 destroyed, bit2 rotated.
- ENTITIES_UPDATE `0x06` uses component mask + optional same_chunk bit.
- CHUNK_SNAPSHOT `0x08`: RAW + occupancy bitset, no RLE.
- CHUNK_DELTA `0x09`: edit list with base_version guard.
- Frame batching: one server frame per tick with multiple submessages.
- No WebSocket compression.

## Architecture Overview

Server (Rust / Tokio / Axum / fastwebsockets)

- `World` task ticks at fixed `tick_rate_hz`.
- `World` owns authoritative entity state and chunk storage.
- Each client has a bounded outbound queue (Bytes) for tick frames.
- Interest management: per-client chunk center + radius; only send relevant
  entities and chunks.

Client (Debug)

- Web client (existing `tools/client.html`) or CLI stub to connect, handshake,
  set interest, and display counts and latency.

## Data Model (v0)

Entity

- id: `u32`
- pos: chunk-relative + chunk coord
- yaw/pitch
- velocity (optional)
- state bits

Chunk

- coords: `i32 cx, cy, cz`
- version: `u32`
- voxel data: 4096 entries, palette-based or direct `u16` ids
- occupancy bitset for snapshots

## Network Protocol (summary)

Frame headers:

- Server frame: `0x10`, `u32 tick`, `u8 submsg_count`
- Client frame: `0x11`, `u32 client_tick_or_seq`, `u8 submsg_count`

Submessages (v0):

- `0x01 HELLO` (client -> server)
- `0x02 WELCOME` (server -> client)
- `0x03 SET_INTEREST` (client -> server)
- `0x04 JOIN`, `0x05 LEAVE` (server -> client)
- `0x06 ENTITIES_UPDATE` (server -> client)
- `0x07 CLIENT_INPUT/POSE` (client -> server)
- `0x08 CHUNK_SNAPSHOT` (server -> client)
- `0x09 CHUNK_DELTA` (server -> client)
- `0x0A CLIENT_CHUNK_REQUEST` (client -> server)
- `0x0B CHUNK_ACK` (client -> server, optional)

## Implementation Steps

Step 0 - Baseline and repo hygiene

- Ensure `src/main.rs` compiles and runs.
- Document current protocol draft and decisions in this file.
- Keep dependencies minimal (Tokio, Axum, fastwebsockets, bytes).

Step 1 - Protocol module (encode/decode)

- Create `src/protocol.rs` with:
    - constants for frame types and submsg kinds
    - enums for submessages
    - encode helpers for LE numeric writes
    - decode helpers with bounds checks
- Add unit tests for each submessage encode/decode.
- Acceptance: round-trip tests pass for all submessages.

Step 2 - Binary handshake and connection state

- Add per-connection state machine: `AwaitHello` -> `Active`.
- Parse `HELLO` from client frames, respond with `WELCOME`.
- Move the current text-based handshake and SetInterest to binary protocol.
- Acceptance: client connects, receives WELCOME with id and tick_rate.

Step 3 - Interest management

- Implement `SET_INTEREST` parsing in binary.
- Store per-client interest and validate bounds (radius limit).
- Add world-side API to update interest by client id.
- Acceptance: client can change AOI and server retains latest values.

Step 4 - Entity model and tick update

- Define `Entity` state in `World`.
- Add server-side integration step per tick (basic movement).
- Track per-client last-sent entity state to compute deltas.
- Build `ENTITIES_UPDATE` each tick for entities in AOI.
- Acceptance: client sees entity updates at tick rate.

Step 5 - Chunk storage and snapshot encoding

- Implement `Chunk` store with version and 4096-voxel array.
- Implement RAW snapshot encoding with occupancy bitset.
- On first interest in a chunk, enqueue `CHUNK_SNAPSHOT`.
- Acceptance: client receives snapshots for visible chunks.

Step 6 - Chunk deltas

- Track edits in chunk (sparse list per tick).
- Send `CHUNK_DELTA` with base_version.
- If client version mismatches, resend snapshot.
- Acceptance: point edits propagate and are applied correctly.

Step 7 - Client input / pose

- Parse `CLIENT_INPUT/POSE` into server state.
- Server integrates movement using input or pose (v0 uses pose).
- Acceptance: player position changes are reflected in updates.

Step 8 - Backpressure and queues

- Implement non-blocking `try_send` for outbound tick data.
- Separate queues (reliable/ephemeral) if needed.
- Add drop policies per message class.
- Acceptance: world tick stays stable under slow clients.

Step 9 - Minimal debug client

- Update `tools/client.html` or add CLI:
    - WebSocket connect
    - HELLO/WELCOME
    - SET_INTEREST
    - display counts for entities/chunks received
- Acceptance: manual end-to-end test with one client.

Step 10 - Testing and profiling

- Add protocol fuzz/edge tests for decode bounds.
- Benchmark encode paths and tick build time.
- Acceptance: no panics on malformed inputs.

Step 11 - Documentation and release

- Keep `SPECIFICATION.md` and `docs/protocol-draft.txt` in sync.
- Add run instructions and sample client usage.
- Acceptance: new contributor can run server + client easily.
