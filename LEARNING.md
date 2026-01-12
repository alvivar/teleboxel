# Learning Notes

## Avoiding Unnecessary Copies: Bytes → BytesMut Conversion

**Date:** 2026-01-12
**Context:** WebSocket payload optimization in `handle_client()`

### The Problem

Original code was forcing an allocation/copy:

```rust
Some(bytes) = rx.recv() => {
    let payload = Payload::Bytes(BytesMut::from(bytes));  // ❌ COPY!
    ws.write_frame(Frame::binary(payload)).await?;
}
```

**Why it copies:**

-   Channel sends `Bytes` (immutable, reference-counted, can be shared)
-   `BytesMut::from(bytes)` requires exclusive ownership
-   Since `Bytes` is designed for shared access, converting to `BytesMut` forces a **full memory copy**

### The Solution

Use `Payload::Borrowed` to create a zero-copy borrow:

```rust
Some(bytes) = rx.recv() => {
    let payload = Payload::Borrowed(&bytes);  // ✅ Zero-copy!
    ws.write_frame(Frame::binary(payload)).await?;
}
```

### Key Learnings

1. **`Bytes` vs `BytesMut`:**

    - `Bytes`: Immutable, cheaply cloneable (ref-counted), designed for sharing
    - `BytesMut`: Mutable, exclusive ownership, designed for building buffers
    - Converting between them often forces copies

2. **When to use each:**

    - Use `Bytes` when you need to share/broadcast the same data to multiple consumers
    - Use `BytesMut` when building buffers or need exclusive mutable access
    - Converting `Bytes → BytesMut` = copy; `BytesMut → Bytes` = cheap (via `.freeze()`)

3. **Borrowed payloads:**

    - `fastwebsockets::Payload::Borrowed(&[u8])` creates a zero-copy payload
    - The lifetime must be valid during the `write_frame()` call
    - Perfect for our use case since `bytes` lives until after `.await`

4. **Design choice for broadcast systems:**
    - Keeping `Bytes` in channels is ideal for game servers that broadcast
    - Can `.clone()` `Bytes` cheaply (just increments ref count)
    - Send same tick data to many players without copying the payload multiple times

### Alternative Approach (Not Chosen)

**Option A:** Store `BytesMut` in the channel instead:

```rust
// Change channel type
mpsc::Sender<BytesMut> / Receiver<BytesMut>

// Direct usage
Some(bytes) = rx.recv() => {
    let payload = Payload::Bytes(bytes);  // No conversion needed
    ws.write_frame(Frame::binary(payload)).await?;
}
```

**Why we didn't choose this:**

-   `BytesMut` can't be cheaply cloned (each clone = full copy)
-   Bad for broadcasting the same data to multiple players
-   Good only if each message is unique per player

### Performance Impact

-   **Before:** Every outbound message triggered a heap allocation + full memory copy
-   **After:** Zero-copy borrow, no allocation
-   **At 60 ticks/sec with 100 players:** Saved 6,000 allocations/second + associated memory bandwidth

### References

-   `bytes` crate: https://docs.rs/bytes/
-   `fastwebsockets` Payload enum: https://docs.rs/fastwebsockets/
-   Rust ownership rules and borrowing fundamentals
