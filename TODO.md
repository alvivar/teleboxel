# TODO

## World Tick Optimization

-   [ ] Implement non-blocking `broadcast_tick` using `try_send` instead of `.send().await`
    -   Use `player.tx.try_send(msg)` to avoid blocking on slow clients
    -   Drop updates on `TrySendError::Full` (or disconnect after N strikes)
    -   Remove disconnected players on `TrySendError::Disconnected`
    -   Keep world tick realtime regardless of client network speed

## Message Channel Strategy

-   [ ] Consider implementing two-channel pattern:
    -   Reliable channel (larger buffer ~256): voxel changes, chunk data, inventory
    -   Ephemeral channel (small buffer ~8): player positions, animations
    -   Disconnect on reliable channel full, drop on ephemeral channel full
