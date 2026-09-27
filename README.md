# Teleboxel

A minimal, fast WebSocket server for a shared voxel world (Rust, Tokio).

The server owns a sparse, infinite voxel grid and a set of replicated
entities. Clients edit the grid and move their entities. The server orders
the changes, stores them and forwards them to the nearby clients.

The code is the specification. Every design reason is a comment next to the
code it explains. The only thing kept outside is the wire contract for client
authors: [PROTOCOL.md](PROTOCOL.md).

## Non-goals

- Auth, anti-cheat, rate limits. Clients are trusted.
- Persistence: the world lives in memory.
- Physics and gameplay rules, which belong in the client.
- More than ~50 concurrent clients.
- Transports other than WebSocket.

## Quick start

```
cargo run --release                                     # server on :3000, stats every 5 s
cargo run --release --bin bot -- 50 127.0.0.1:3000 60   # 50 bots for 60 s
```

Bot usage:
`bot [clients=50] [addr=127.0.0.1:3000] [seconds=30] [view_xz=8] [view_y=4] [edits_per_sec=2]`.

`tools/client.html` is a 2D debug client. Open the file in a browser and
press Connect. It draws only what the server sent.

## Layout

- `src/protocol.rs`: the wire format.
- `src/main.rs`: HTTP/WebSocket accept and the connection lifecycle.
- `src/world.rs`: world state, the tick and per-client sync.
- `src/bin/bot.rs`: the load bot.

A visual tour of how they fit together: [docs/overview.html](docs/overview.html).

## Measured

With 50 bots on the same machine as the server (i7-13700H, Windows 11,
loopback), a tick takes 0.5–0.8 ms of work on average, within the 1 ms
target. The worst ticks, 2–4 ms steady and ~10 ms while all 50 clients join,
are accepted: the tick period is 33 ms.

## Deferred

Optimizations, measure first; chunk traffic dominates:

- compact chunk encodings (uniform, RLE, palette);
- snapshot encodings shared between clients;
- zero-allocation frame building.

Features:

- drop-tolerant sync (versioned state), for UDP/WebTransport;
- `PING`/`PONG` for client RTT;
- several entities per client;
- prediction and reconciliation;
- validation, rate limits, auth;
- persistence.
