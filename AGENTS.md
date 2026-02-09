# AGENTS.md

Guide for AI coding agents contributing to **Teleboxel**.

## Project mission

Teleboxel aims to be a **fast, authoritative voxel + player sync server** over WebSockets in Rust (Tokio ecosystem), suitable for Minecraft-like multiplayer games.

Current state is an early prototype with working networking/plumbing, but without the binary protocol implementation yet.

---

## Read this first (in order)

1. `README.md` — short project intent
2. `STATUS.md` — what works now vs missing
3. `SPECIFICATION.md` — canonical implementation plan (v0)
4. `docs/protocol-draft.txt` — protocol design details
5. `LEARNING.md` — performance gotcha (Bytes/zero-copy)
6. `TODO.md` — immediate optimization tasks
7. `src/main.rs` — current running server

`SPECIFICATION.md` is the source of truth for concrete v0 decisions. The protocol draft is broader/reference-level.

---

## Repository map

- `src/main.rs` — entire server prototype (world task + websocket handling)
- `docs/protocol-draft.txt` — detailed protocol design draft
- `tools/client.html` — manual browser websocket test client (currently text-oriented)
- `SPECIFICATION.md` — step-by-step implementation roadmap
- `STATUS.md` — current progress snapshot
- `TODO.md` — focused near-term tasks
- `LEARNING.md` — implementation/performance notes
- `Cargo.toml` — minimal deps

---

## Tech stack and constraints

- Rust edition: **2024**
- Dependencies intentionally minimal:
    - `tokio`
    - `axum` (ws feature)
    - `fastwebsockets` (upgrade + with_axum)
    - `bytes`
- Keep dependency growth conservative unless clearly justified.

Protocol/runtime constraints (v0):

- Authoritative server tick loop
- Binary protocol target with fixed-width LE fields
- Chunk size: `16x16x16`
- No websocket compression initially
- No auth, persistence, advanced physics (non-goals for v0)

---

## How to run

```bash
cargo run
```

Server listens on `0.0.0.0:3000` and exposes websocket endpoint at `/`.

Quick manual client path:

1. Open `tools/client.html` in a browser.
2. Connect to `ws://localhost:3000`.
3. Send text command (current prototype):
    - `SetInterest 0 0 0 4`

---

## Current implementation (important reality check)

### Implemented now

- Websocket upgrade via Axum + fastwebsockets
- Dedicated `World` task with fixed tick loop
- Player connect/disconnect wiring through `WorldMsg`
- Per-player bounded outbound channel (`mpsc::channel<Bytes>(128)`)
- Temporary text command parsing for `SetInterest`
- Zero-copy outbound websocket payload path using `Payload::Borrowed(&bytes)`

### Not implemented yet

- Binary frame/submessage protocol parsing/encoding
- HELLO/WELCOME binary handshake
- Entity state simulation and `ENTITIES_UPDATE`
- Chunk storage/snapshot/delta transport
- Input/pose integration
- Backpressure policy (`try_send`) and queue classes

---

## Architecture snapshot (`src/main.rs`)

- `WorldMsg`: `Connect`, `Disconnect`, `SetInterest`
- `World`:
    - owns player map and id allocation
    - runs fixed-tick loop (`world.run(60)` currently)
    - handles world messages and (future) broadcasting
- `handle_client`:
    - requests connect from world (oneshot reply returns `id` + outbound receiver)
    - sends text handshake (currently just `id` string)
    - parses text `SetInterest x y z radius`
    - forwards world updates from channel to websocket as binary frames

Tick loop behavior:

- Uses `tokio::time::interval`
- `MissedTickBehavior::Skip`
- On each tick: drain queued world messages via `try_recv`, then call `broadcast_tick()`
- Also handles low-latency message receive path between ticks

---

## Protocol target (v0 summary)

Frame types:

- Server frame: `0x10` (`tick`, `submsg_count`)
- Client frame: `0x11` (`client_tick_or_seq`, `submsg_count`)

Submessages:

- `0x01 HELLO` (C→S)
- `0x02 WELCOME` (S→C)
- `0x03 SET_INTEREST` (C→S)
- `0x04 JOIN`, `0x05 LEAVE` (S→C)
- `0x06 ENTITIES_UPDATE` (S→C)
- `0x07 CLIENT_INPUT/POSE` (C→S)
- `0x08 CHUNK_SNAPSHOT` (S→C)
- `0x09 CHUNK_DELTA` (S→C)
- `0x0A CLIENT_CHUNK_REQUEST` (C→S)
- `0x0B CHUNK_ACK` optional (C→S)

Concrete v0 decisions are documented in `SPECIFICATION.md` (use them).

---

## Coding guidance for contributors

1. **Preserve authoritative world model**
    - Connection handlers should not own gameplay state.
    - World task remains source of truth.

2. **Avoid unnecessary allocations/copies**
    - Prefer `Bytes` for sharable outbound data.
    - Avoid `Bytes -> BytesMut` conversion on hot paths unless required.
    - See `LEARNING.md` for rationale.

3. **Plan for backpressure explicitly**
    - Move toward `try_send` in tick broadcast path.
    - Slow clients must not stall world tick.

4. **Binary protocol parsing must be defensive**
    - Bounds checks everywhere; malformed input must not panic.

5. **Keep docs synchronized**
    - If protocol semantics change, update:
        - `SPECIFICATION.md`
        - `docs/protocol-draft.txt` (if relevant)
        - `STATUS.md` / `TODO.md` as needed

6. **Keep changes incremental**
    - Small, testable steps aligned with `SPECIFICATION.md` phases.

---

## Immediate high-priority tasks

From `STATUS.md` + `TODO.md` + `SPECIFICATION.md`:

1. Create binary protocol module (`src/protocol.rs`) with encode/decode helpers.
2. Replace text handshake with binary `HELLO/WELCOME`.
3. Parse binary `SET_INTEREST` and store validated AOI.
4. Implement basic entity model + `ENTITIES_UPDATE` at tick rate.
5. Implement `try_send`/drop policy so world tick is never blocked by clients.

---

## Validation checklist before finishing a change

- `cargo check` passes
- If behavior changed, manual websocket flow still works
- No obvious hot-path copying introduced
- Docs updated when protocol/architecture decisions changed
- Changes match current phase in `SPECIFICATION.md`

---

## Known warnings / debt (current)

At prototype stage, `cargo check` currently warns about:

- Unused loop variable in `broadcast_tick`
- `Player.tx` currently not read

These are expected while broadcast logic is incomplete.

---

## Notes for AI agents

- Prefer editing existing architecture over introducing parallel systems.
- Do not add persistence/auth/complex physics unless explicitly requested.
- Keep protocol implementation LE/fixed-width for v0.
- Favor throughput/latency-aware decisions (batching, bounded queues, minimal copies).
