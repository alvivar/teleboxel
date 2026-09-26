# Status

v0 is complete. `SPECIFICATION.md` is the source of truth; this file only says
where things stand.

## What we have

- `src/protocol.rs`: binary wire format (§7). Decodes client messages with
  explicit errors, encodes server messages.
- `src/main.rs`: WebSocket connection lifecycle (§8): split reader/writer,
  `HELLO`, `Join`/`Leave`, close 1002 on malformed input, teardown even when a
  client stops reading.
- `src/world.rs`: the 30 Hz world tick (§5, §6): entity and sparse chunk
  replication with view hysteresis, reliable-or-disconnect outbound queues,
  and stats every 5 s (§11).
- `src/bin/bot.rs`: load tool that simulates N clients.
- `tools/client.html`: 2D debug client. It shows only what the server sent.

## Measured

Tick work averages 0.5–0.8 ms with 50 bots, which meets the target in §11.
Details are in §11.1.

## How to run

```
cargo run --release                                     # server on :3000
cargo run --release --bin bot -- 50 127.0.0.1:3000 60   # 50 bots for 60 s
```

Open `tools/client.html` in a browser and press Connect. If `localhost` does
not connect, use `ws://127.0.0.1:3000`.

## Next

Nothing is scheduled. Candidates, only if a measurement or a feature asks for
them, are listed in §13.
