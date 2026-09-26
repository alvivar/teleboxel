//! World state, tick and per-client sync (SPECIFICATION.md §5, §6). Chunks
//! come in the next step (§12).

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
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

pub const TICK_HZ: u8 = 30;
pub const OUTBOUND_QUEUE: usize = 32;
pub const INBOUND_QUEUE: usize = 1024;

/// Connection → world. Validated by the connection; the world trusts it.
pub enum Command {
    Join {
        id: u32,
        tx: mpsc::Sender<Bytes>,
        /// Never sent. The world drops it when it forgets the client, which
        /// tells the connection to close.
        disconnect: oneshot::Sender<()>,
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
    _disconnect: oneshot::Sender<()>,
    view_h: u8,
    view_v: u8,
    entity: Option<Entity>,
    known_entities: HashSet<u32>,
}

/// The client's entity; its id is the client id (§10).
struct Entity {
    pos: [i32; 3],
    data: Vec<u8>,
}

pub struct World {
    rx: mpsc::Receiver<Command>,
    clients: HashMap<u32, Client>,
    tick: u32,
    changed: HashSet<u32>,
    destroyed: Vec<u32>,
}

impl World {
    pub fn new(rx: mpsc::Receiver<Command>) -> Self {
        Self {
            rx,
            clients: HashMap::new(),
            tick: 0,
            changed: HashSet::new(),
            destroyed: Vec::new(),
        }
    }

    pub async fn run(mut self) {
        let mut ticker = time::interval(Duration::from_secs(1) / u32::from(TICK_HZ));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            self.step();
        }
    }

    fn step(&mut self) {
        while let Ok(command) = self.rx.try_recv() {
            self.apply(command);
        }
        let disconnected = self.send_frames();
        self.changed.clear();
        self.destroyed.clear();
        // After the clear, so the others get ENTITY_REMOVE next tick.
        for id in disconnected {
            self.remove(id);
        }
        self.tick += 1;
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
                let mut frame = BytesMut::new();
                protocol::put_tick(&mut frame, self.tick);
                protocol::put_welcome(&mut frame, id, TICK_HZ);
                // A new queue cannot be full. If it is closed, the connection
                // already ended and its `Leave` follows.
                if tx.try_send(frame.freeze()).is_ok() {
                    let client = Client {
                        tx,
                        _disconnect: disconnect,
                        view_h,
                        view_v,
                        entity: None,
                        known_entities: HashSet::new(),
                    };
                    self.clients.insert(id, client);
                }
            }
            Command::Leave { id } => self.remove(id),
            Command::Message { id, message } => {
                // Unknown once the world has disconnected it (§5.1); its
                // `Leave` follows.
                let Some(client) = self.clients.get_mut(&id) else {
                    return;
                };
                match message {
                    ClientMessage::EntityState { pos, data } => {
                        client.entity = Some(Entity { pos, data });
                        self.changed.insert(id);
                    }
                    // Chunks: §12 step 4.
                    ClientMessage::VoxelEdits(_) => {}
                }
            }
        }
    }

    /// Removes a client and destroys its entity. The id is already unknown
    /// when the world disconnected the client before its `Leave` arrived.
    fn remove(&mut self, id: u32) {
        if self.clients.remove(&id).is_some() {
            self.destroyed.push(id);
        }
    }

    /// Builds and sends each client's frame (§5.2). Returns the clients to
    /// disconnect (§5.1).
    fn send_frames(&mut self) -> Vec<u32> {
        // Each entity's ENTITY_STATE, encoded once for every viewer.
        let entities: Vec<(u32, [i32; 3], Bytes)> = self
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
            // No entity, no view (§3.3).
            let Some(entity) = &client.entity else {
                continue;
            };
            let center = entity_chunk(entity.pos);
            let (h, v) = (i32::from(client.view_h), i32::from(client.view_v));
            let known = &mut client.known_entities;

            let mut frame = BytesMut::new();
            protocol::put_tick(&mut frame, self.tick);
            let header_len = frame.len();

            for &other in &self.destroyed {
                if known.remove(&other) {
                    protocol::put_entity_remove(&mut frame, other);
                }
            }
            for (other, chunk, state) in &entities {
                if *other == id {
                    continue;
                }
                if !known.contains(other) {
                    if in_box(center, *chunk, h, v) {
                        frame.extend_from_slice(state);
                        known.insert(*other);
                    }
                } else if !in_box(center, *chunk, h + 1, v + 1) {
                    protocol::put_entity_remove(&mut frame, *other);
                    known.remove(other);
                } else if self.changed.contains(other) {
                    frame.extend_from_slice(state);
                }
            }

            if frame.len() == header_len {
                continue;
            }
            match client.tx.try_send(frame.freeze()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    eprintln!("client {id}: outbound queue full, disconnecting");
                    disconnected.push(id);
                }
                // The connection already ended.
                Err(TrySendError::Closed(_)) => disconnected.push(id),
            }
        }
        disconnected
    }
}

/// §4: entity positions are 1/256 voxel, chunks are 16 voxels.
fn entity_chunk([x, y, z]: [i32; 3]) -> [i32; 3] {
    [x >> 12, y >> 12, z >> 12]
}

/// Whether chunk `p` is within `h` horizontally and `v` vertically of `c` (§3.3).
fn in_box(c: [i32; 3], p: [i32; 3], h: i32, v: i32) -> bool {
    (p[0] - c[0]).abs() <= h && (p[2] - c[2]).abs() <= h && (p[1] - c[1]).abs() <= v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join(world: &mut World, id: u32, queue: usize) -> mpsc::Receiver<Bytes> {
        let (tx, mut rx) = mpsc::channel(queue);
        let (disconnect, _) = oneshot::channel();
        world.apply(Command::Join {
            id,
            tx,
            disconnect,
            view_h: 1,
            view_v: 1,
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
        let mut a = join(&mut world, 1, OUTBOUND_QUEUE);
        let _b = join(&mut world, 2, OUTBOUND_QUEUE);
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
        let mut a = join(&mut world, 1, OUTBOUND_QUEUE);
        let _b = join(&mut world, 2, 1);
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
}
