//! World state, tick and per-client sync. The world produces `Bytes` frames
//! and knows nothing about WebSocket, but it assumes an ordered, reliable
//! stream (see `World::send_frames`).

use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    mem,
    time::{Duration, Instant},
};

use bytes::{Bytes, BytesMut};
use tokio::{
    sync::{
        mpsc::{self, error::TrySendError},
        oneshot,
    },
    time::{self, MissedTickBehavior},
};

use crate::protocol::{self, ClientMessage};

/// A common game rate; clients interpolate between ticks.
pub const TICK_HZ: u8 = 30;
/// Frames per client, about 1 s at `TICK_HZ`. A client this far behind is
/// disconnected.
pub const OUTBOUND_QUEUE: usize = 32;
/// Commands from every connection to the world.
pub const INBOUND_QUEUE: usize = 1024;
/// Per client, view-fill snapshots from `pending` only: at most 64 KiB per
/// frame, about 2 MiB/s. Chunks sent because of edits are not capped.
const MAX_SNAPSHOTS_PER_TICK: usize = 8;
/// Above this, a snapshot is sent instead of the edits: `count` stays inside
/// a `u16`, and a delta stays roughly snapshot-sized (2048 × 4 B = 8 KiB).
const MAX_CHUNK_EDITS: usize = 2048;
const STATS_PERIOD: Duration = Duration::from_secs(5);

type ChunkPos = [i32; 3];

/// Connection → world. Validated by the connection; the world trusts it.
pub enum Command {
    Join {
        id: u32,
        tx: mpsc::Sender<Bytes>,
        /// Tells the connection to close: the world drops it when it forgets
        /// the client, after sending the reason if the world disconnected the
        /// client itself.
        disconnect: oneshot::Sender<&'static str>,
        view_h: u8,
        view_v: u8,
    },
    Leave {
        id: u32,
    },
    Message {
        id: u32,
        message: ClientMessage,
    },
}

struct Client {
    tx: mpsc::Sender<Bytes>,
    disconnect: oneshot::Sender<&'static str>,
    view_h: i32,
    view_v: i32,
    entity: Option<Entity>,
    /// The center chunk of the last view scan (see `sync_chunks`).
    view_center: Option<ChunkPos>,
    /// What the client has; no versions (see `World::send_frames`).
    known_chunks: HashSet<ChunkPos>,
    /// Chunks in view the client does not have yet, nearest last, so `pop`
    /// takes the nearest.
    pending: Vec<ChunkPos>,
    known_entities: HashSet<u32>,
}

/// The client's entity; its id is the client id. One record per client:
/// several entities per client would first split them.
struct Entity {
    pos: [i32; 3],
    data: Vec<u8>,
}

/// Accumulated over one `STATS_PERIOD`. Only `step` is timed, not the wait
/// for the next tick.
#[derive(Default)]
struct Stats {
    ticks: u32,
    work: Duration,
    max_work: Duration,
    bytes_out: usize,
}

struct Chunk {
    blocks: [u16; 4096],
    /// `(index, block)` edits of this tick.
    edits: Vec<(u16, u16)>,
}

pub struct World {
    rx: mpsc::Receiver<Command>,
    clients: HashMap<u32, Client>,
    chunks: HashMap<ChunkPos, Chunk>,
    tick: u32,
    edited: Vec<ChunkPos>,
    changed: HashSet<u32>,
    destroyed: Vec<u32>,
    stats: Stats,
}

impl World {
    pub fn new(rx: mpsc::Receiver<Command>) -> Self {
        Self {
            rx,
            clients: HashMap::new(),
            chunks: HashMap::new(),
            tick: 0,
            edited: Vec::new(),
            changed: HashSet::new(),
            destroyed: Vec::new(),
            stats: Stats::default(),
        }
    }

    pub async fn run(mut self) {
        let mut ticker = time::interval(Duration::from_secs(1) / u32::from(TICK_HZ));
        // A late tick is skipped, not made up with a burst.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut period_start = Instant::now();
        loop {
            ticker.tick().await;
            let start = Instant::now();
            self.step();
            let work = start.elapsed();
            self.stats.ticks += 1;
            self.stats.work += work;
            self.stats.max_work = self.stats.max_work.max(work);

            let period = period_start.elapsed();
            if period >= STATS_PERIOD {
                self.print_stats(period);
                period_start = Instant::now();
            }
        }
    }

    fn print_stats(&mut self, period: Duration) {
        let stats = mem::take(&mut self.stats);
        let entities = self.clients.values().filter(|c| c.entity.is_some()).count();
        println!(
            "tick avg {:.2?} max {:.2?} | out {:.1} KiB/s | clients {} entities {} chunks {}",
            stats.work / stats.ticks,
            stats.max_work,
            stats.bytes_out as f64 / 1024.0 / period.as_secs_f64(),
            self.clients.len(),
            entities,
            self.chunks.len(),
        );
    }

    fn step(&mut self) {
        // Commands are only read here: nothing is sent between ticks, so
        // reading them earlier would only add wakeups.
        while let Ok(command) = self.rx.try_recv() {
            self.apply(command);
        }
        let disconnected = self.send_frames();
        for pos in self.edited.drain(..) {
            let chunk = self.chunks.get_mut(&pos).expect("chunks are never removed");
            chunk.edits.clear();
        }
        self.changed.clear();
        self.destroyed.clear();
        // After the clear, so the others get ENTITY_REMOVE next tick.
        for (id, reason) in disconnected {
            let client = self.remove(id).expect("disconnected clients are known");
            if let Some(reason) = reason {
                // Fails only if the connection has already ended.
                let _ = client.disconnect.send(reason);
            }
        }
        // After ~4.5 years at 30 Hz. PROTOCOL.md tells clients.
        self.tick = self.tick.wrapping_add(1);
    }

    fn apply(&mut self, command: Command) {
        match command {
            Command::Join {
                id,
                tx,
                disconnect,
                view_h,
                view_v,
            } => {
                // Sent by the world, not the connection, so that even this
                // first frame carries a real tick.
                let mut frame = BytesMut::new();
                protocol::put_tick(&mut frame, self.tick);
                protocol::put_welcome(&mut frame, id, TICK_HZ);
                // A new queue cannot be full. If it is closed, the connection
                // already ended and its `Leave` follows.
                let frame = frame.freeze();
                let len = frame.len();
                if tx.try_send(frame).is_ok() {
                    self.stats.bytes_out += len;
                    let client = Client {
                        tx,
                        disconnect,
                        view_h: i32::from(view_h),
                        view_v: i32::from(view_v),
                        entity: None,
                        view_center: None,
                        known_chunks: HashSet::new(),
                        pending: Vec::new(),
                        known_entities: HashSet::new(),
                    };
                    self.clients.insert(id, client);
                }
            }
            Command::Leave { id } => {
                self.remove(id);
            }
            Command::Message { id, message } => {
                // Unknown once the world has disconnected it; its `Leave`
                // follows.
                let Some(client) = self.clients.get_mut(&id) else {
                    return;
                };
                match message {
                    ClientMessage::EntityState { pos, data } => {
                        client.entity = Some(Entity { pos, data });
                        self.changed.insert(id);
                    }
                    ClientMessage::VoxelEdits(edits) => {
                        for edit in edits {
                            let (pos, index) = voxel_chunk_and_index(edit.pos);
                            let chunk = self.chunks.entry(pos).or_insert_with(|| Chunk {
                                blocks: [0; 4096],
                                edits: Vec::new(),
                            });
                            if chunk.edits.is_empty() {
                                self.edited.push(pos);
                            }
                            chunk.blocks[usize::from(index)] = edit.block;
                            chunk.edits.push((index, edit.block));
                        }
                    }
                }
            }
        }
    }

    /// Removes a client and destroys its entity. The id is already unknown
    /// when the world disconnected the client before its `Leave` arrived.
    fn remove(&mut self, id: u32) -> Option<Client> {
        let client = self.clients.remove(&id)?;
        self.destroyed.push(id);
        Some(client)
    }

    /// Builds and sends each client's frame. Returns the clients to disconnect,
    /// with the reason for their connection if it does not know it already.
    ///
    /// The stream is reliable, or the client is gone: `try_send` never awaits
    /// the client, and a full queue disconnects it instead of dropping the
    /// frame. So the world only remembers what each client has, never which
    /// version. Dropping frames, as an unreliable transport would, needs
    /// versioned state per client.
    fn send_frames(&mut self) -> Vec<(u32, Option<&'static str>)> {
        // Each entity's ENTITY_STATE, encoded once for every viewer.
        let entities: Vec<(u32, ChunkPos, Bytes)> = self
            .clients
            .iter()
            .filter_map(|(&id, client)| {
                let entity = client.entity.as_ref()?;
                let mut state = BytesMut::new();
                protocol::put_entity_state(&mut state, id, entity.pos, &entity.data);
                Some((id, entity_chunk(entity.pos), state.freeze()))
            })
            .collect();

        let mut disconnected = Vec::new();
        for (&id, client) in &mut self.clients {
            // No entity, no view: nothing but WELCOME.
            let Some(entity) = &client.entity else {
                continue;
            };
            let center = entity_chunk(entity.pos);

            let mut frame = BytesMut::new();
            protocol::put_tick(&mut frame, self.tick);
            let header_len = frame.len();
            client.sync_chunks(center, &self.chunks, &self.edited, &mut frame);
            client.sync_entities(
                id,
                center,
                &entities,
                &self.changed,
                &self.destroyed,
                &mut frame,
            );
            // Nothing to send, no frame.
            if frame.len() == header_len {
                continue;
            }

            let len = frame.len();
            match client.tx.try_send(frame.freeze()) {
                Ok(()) => self.stats.bytes_out += len,
                Err(TrySendError::Full(_)) => {
                    disconnected.push((id, Some("outbound queue full")));
                }
                // The connection already ended, and knows why.
                Err(TrySendError::Closed(_)) => disconnected.push((id, None)),
            }
        }
        disconnected
    }
}

impl Client {
    /// The chunk part of this tick's frame. The work is bounded by what
    /// changed, not by the view size.
    fn sync_chunks(
        &mut self,
        center: ChunkPos,
        chunks: &HashMap<ChunkPos, Chunk>,
        edited: &[ChunkPos],
        frame: &mut BytesMut,
    ) {
        let (h, v) = (self.view_h, self.view_v);
        // Scanning the view every tick is up to 33 × 33 × 17 lookups per
        // client, over the 1 ms tick target at 50 clients. So the view is only
        // scanned when the entity crosses a chunk boundary.
        if self.view_center != Some(center) {
            self.view_center = Some(center);
            // Hysteresis: what the client has stays within H + 1, or moving
            // back and forth across a border would unload and resend a whole
            // plane of chunks every time.
            self.known_chunks.retain(|&pos| {
                let keep = in_box(center, pos, h + 1, v + 1);
                if !keep {
                    protocol::put_chunk_unload(frame, pos);
                }
                keep
            });
            self.pending.clear();
            for y in -v..=v {
                for z in -h..=h {
                    for x in -h..=h {
                        let pos = [center[0] + x, center[1] + y, center[2] + z];
                        if chunks.contains_key(&pos) && !self.known_chunks.contains(&pos) {
                            self.pending.push(pos);
                        }
                    }
                }
            }
            self.pending
                .sort_unstable_by_key(|&pos| Reverse(distance_squared(center, pos)));
        }

        for pos in edited {
            let chunk = &chunks[pos];
            // Wherever it is: a known chunk in the hysteresis ring is never
            // resent, so without its edits it would come back into view stale.
            if self.known_chunks.contains(pos) {
                if chunk.edits.len() > MAX_CHUNK_EDITS {
                    protocol::put_chunk(frame, *pos, &chunk.blocks);
                } else {
                    protocol::put_chunk_edits(frame, *pos, &chunk.edits);
                }
            } else if in_box(center, *pos, h, v) {
                protocol::put_chunk(frame, *pos, &chunk.blocks);
                self.known_chunks.insert(*pos);
            }
        }

        // Only to an empty queue, i.e. while the writer keeps up: otherwise a
        // slow link fills up with snapshots and entity updates wait behind
        // them. Capped, because entering a built area can mean megabytes.
        // Nearest first, or far chunks could arrive before near ones.
        if self.tx.capacity() == self.tx.max_capacity() {
            let mut sent = 0;
            while sent < MAX_SNAPSHOTS_PER_TICK
                && let Some(pos) = self.pending.pop()
            {
                // Skipped if an edit already sent it.
                if self.known_chunks.insert(pos) {
                    protocol::put_chunk(frame, pos, &chunks[&pos].blocks);
                    sent += 1;
                }
            }
        }
    }

    /// The entity part of this tick's frame. Checked every tick, because
    /// entities move on their own. Same view and hysteresis as the chunks.
    fn sync_entities(
        &mut self,
        id: u32,
        center: ChunkPos,
        entities: &[(u32, ChunkPos, Bytes)],
        changed: &HashSet<u32>,
        destroyed: &[u32],
        frame: &mut BytesMut,
    ) {
        let (h, v) = (self.view_h, self.view_v);
        let known = &mut self.known_entities;
        for &other in destroyed {
            if known.remove(&other) {
                protocol::put_entity_remove(frame, other);
            }
        }
        for (other, chunk, state) in entities {
            if *other == id {
                continue;
            }
            if !known.contains(other) {
                if in_box(center, *chunk, h, v) {
                    frame.extend_from_slice(state);
                    known.insert(*other);
                }
            } else if !in_box(center, *chunk, h + 1, v + 1) {
                protocol::put_entity_remove(frame, *other);
                known.remove(other);
            } else if changed.contains(other) {
                frame.extend_from_slice(state);
            }
        }
    }
}

/// Entity positions are 1/256 voxel (24.8), chunks are 16 voxels.
fn entity_chunk([x, y, z]: [i32; 3]) -> ChunkPos {
    [x >> 12, y >> 12, z >> 12]
}

/// The voxel's chunk and its index inside it, Y-major so that each horizontal
/// layer is contiguous.
fn voxel_chunk_and_index([x, y, z]: [i32; 3]) -> (ChunkPos, u16) {
    let index = (x & 15) | ((z & 15) << 4) | ((y & 15) << 8);
    ([x >> 4, y >> 4, z >> 4], index as u16)
}

/// Whether chunk `p` is within `h` horizontally and `v` vertically of `c`.
fn in_box(c: ChunkPos, p: ChunkPos, h: i32, v: i32) -> bool {
    (p[0] - c[0]).abs() <= h && (p[2] - c[2]).abs() <= h && (p[1] - c[1]).abs() <= v
}

fn distance_squared(c: ChunkPos, p: ChunkPos) -> i32 {
    (0..3).map(|i| (p[i] - c[i]).pow(2)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::VoxelEdit;

    fn join(world: &mut World, id: u32, queue: usize, view: u8) -> mpsc::Receiver<Bytes> {
        let (tx, mut rx) = mpsc::channel(queue);
        let (disconnect, _) = oneshot::channel();
        world.apply(Command::Join {
            id,
            tx,
            disconnect,
            view_h: view,
            view_v: view,
        });
        rx.try_recv().expect("WELCOME");
        rx
    }

    fn move_to(world: &mut World, id: u32, chunk_x: i32) {
        let message = ClientMessage::EntityState {
            pos: [chunk_x << 12, 0, 0],
            data: vec![7],
        };
        world.apply(Command::Message { id, message });
    }

    fn edit(world: &mut World, id: u32, voxels: &[[i32; 3]], block: u16) {
        let edits = voxels.iter().map(|&pos| VoxelEdit { pos, block }).collect();
        let message = ClientMessage::VoxelEdits(edits);
        world.apply(Command::Message { id, message });
    }

    fn frame(tick: u32, messages: impl FnOnce(&mut BytesMut)) -> Bytes {
        let mut frame = BytesMut::new();
        protocol::put_tick(&mut frame, tick);
        messages(&mut frame);
        frame.freeze()
    }

    /// Risk: an off-by-one in the view or hysteresis box.
    #[test]
    fn entity_enters_view_stays_within_hysteresis_and_leaves_beyond() {
        let mut world = World::new(mpsc::channel(1).1);
        let mut a = join(&mut world, 1, OUTBOUND_QUEUE, 1);
        let _b = join(&mut world, 2, OUTBOUND_QUEUE, 1);
        move_to(&mut world, 1, 0);
        let state = |frame: &mut BytesMut, x: i32| {
            protocol::put_entity_state(frame, 2, [x << 12, 0, 0], &[7]);
        };

        // (B's chunk x, what A receives)
        let steps: [(i32, Option<Bytes>); 5] = [
            (1, Some(frame(0, |f| state(f, 1)))), // enters view (H = 1)
            (2, Some(frame(1, |f| state(f, 2)))), // known, changed, within H + 1
            (3, Some(frame(2, |f| protocol::put_entity_remove(f, 2)))), // beyond H + 1
            (2, None),                            // not known, outside H
            (1, Some(frame(4, |f| state(f, 1)))), // enters view again
        ];
        for (x, expected) in steps {
            move_to(&mut world, 2, x);
            world.step();
            assert_eq!(a.try_recv().ok(), expected, "B at chunk x = {x}");
        }
        // Nothing changed: no frame.
        world.step();
        assert!(a.try_recv().is_err());
    }

    /// Risk: a client disconnected by the world stays visible to the others.
    #[test]
    fn full_queue_disconnects_and_others_get_entity_remove() {
        let mut world = World::new(mpsc::channel(1).1);
        let mut a = join(&mut world, 1, OUTBOUND_QUEUE, 1);
        let _b = join(&mut world, 2, 1, 1);
        move_to(&mut world, 1, 0);
        move_to(&mut world, 2, 0);
        world.step(); // Both see each other; B's queue is now full.
        a.try_recv().expect("ENTITY_STATE of B");

        move_to(&mut world, 1, 1);
        world.step(); // B's frame does not fit: B is disconnected.
        assert!(!world.clients.contains_key(&2));

        world.step();
        let remove = frame(2, |f| protocol::put_entity_remove(f, 2));
        assert_eq!(a.try_recv().ok(), Some(remove));

        // Its late `Leave` and messages are harmless.
        world.apply(Command::Leave { id: 2 });
        move_to(&mut world, 2, 0);
        world.step();
        assert!(a.try_recv().is_err());
    }

    /// Risks: a known chunk in the hysteresis ring going stale, a chunk sent
    /// twice in one frame, and a `count` overflowing `u16`.
    #[test]
    fn chunk_edits_follow_known_chunks_until_unload() {
        let mut world = World::new(mpsc::channel(1).1);
        let mut a = join(&mut world, 1, OUTBOUND_QUEUE, 1);
        let pos = [1, 0, 0];
        let blocks = |world: &World| world.chunks[&pos].blocks;

        // The first scan finds the new chunk in `pending`; the edit already sends it.
        move_to(&mut world, 1, 0);
        edit(&mut world, 1, &[[16, 0, 0]], 5);
        world.step();
        let expected = frame(0, |f| protocol::put_chunk(f, pos, &blocks(&world)));
        assert_eq!(a.try_recv().ok(), Some(expected));

        // At distance H + 1 the chunk stays known and still gets edits.
        move_to(&mut world, 1, -1);
        edit(&mut world, 1, &[[17, 0, 0]], 6);
        world.step();
        let expected = frame(1, |f| protocol::put_chunk_edits(f, pos, &[(1, 6)]));
        assert_eq!(a.try_recv().ok(), Some(expected));

        let voxels: Vec<[i32; 3]> = (0..=MAX_CHUNK_EDITS as i32)
            .map(|i| [16 + i % 16, i / 256, i / 16 % 16])
            .collect();
        edit(&mut world, 1, &voxels, 9);
        world.step();
        let expected = frame(2, |f| protocol::put_chunk(f, pos, &blocks(&world)));
        assert_eq!(a.try_recv().ok(), Some(expected));

        move_to(&mut world, 1, -2);
        world.step();
        let expected = frame(3, |f| protocol::put_chunk_unload(f, pos));
        assert_eq!(a.try_recv().ok(), Some(expected));
    }

    /// Risk: entering a built area floods the client, or sends far chunks first.
    #[test]
    fn pending_chunks_go_nearest_first_capped_and_only_to_an_empty_queue() {
        let mut world = World::new(mpsc::channel(1).1);
        let mut a = join(&mut world, 1, OUTBOUND_QUEUE, 4);
        let _builder = join(&mut world, 2, OUTBOUND_QUEUE, 4);
        // Distinct distances from chunk 0, nearest first.
        let chunks: [ChunkPos; 10] = [
            [0, 0, 0],
            [1, 0, 0],
            [2, 0, 0],
            [3, 0, 0],
            [4, 0, 0],
            [4, 1, 0],
            [4, 2, 0],
            [4, 3, 0],
            [4, 4, 0],
            [4, 4, 1],
        ];
        let voxels: Vec<[i32; 3]> = chunks.iter().map(|p| p.map(|c| c * 16)).collect();
        edit(&mut world, 2, &voxels, 1);
        world.step(); // Neither client has an entity: nothing is sent.
        assert!(a.try_recv().is_err());

        move_to(&mut world, 1, 0);
        world.step();
        world.step(); // The first frame is still queued: no snapshots.
        let snapshots = |world: &World, tick, chunks: &[ChunkPos]| {
            frame(tick, |f| {
                for pos in chunks {
                    protocol::put_chunk(f, *pos, &world.chunks[pos].blocks);
                }
            })
        };
        assert_eq!(a.try_recv().ok(), Some(snapshots(&world, 1, &chunks[..8])));
        assert!(a.try_recv().is_err());

        world.step();
        assert_eq!(a.try_recv().ok(), Some(snapshots(&world, 3, &chunks[8..])));
    }
}
