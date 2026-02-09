# Status

## What we have

- Working WebSocket server using Axum + fastwebsockets.
- `World` task with fixed tick loop and player registry.
- Connect / Disconnect handling via `WorldMsg`.
- Text-based `SetInterest` command (temporary).
- Per-player outbound `Bytes` channel and zero-copy send path.
- Protocol draft documented in `docs/protocol-draft.txt`.

## Where we are

- Prototype stage: network plumbing exists, but binary protocol is not
  implemented.
- World tick does not yet broadcast any real state.
- Client path is still text based and only sets interest.

## What we need (next)

- Implement binary protocol encode/decode module.
- Replace text handshake with `HELLO` / `WELCOME`.
- Parse binary `SET_INTEREST` and store per-client AOI.
- Build entity model + per-tick `ENTITIES_UPDATE`.
- Add chunk storage, snapshots, and deltas.
- Add client input/pose handling.
- Add backpressure logic for outbound queues.
- Build or update debug client for end-to-end tests.
