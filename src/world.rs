//! World state and tick (SPECIFICATION.md §6). Entities and chunks come in
//! the next steps (§12).

use std::{collections::HashMap, time::Duration};

use bytes::{Bytes, BytesMut};
use tokio::{
    sync::mpsc,
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
    view_h: u8,
    view_v: u8,
}

pub struct World {
    rx: mpsc::Receiver<Command>,
    clients: HashMap<u32, Client>,
    tick: u32,
}

impl World {
    pub fn new(rx: mpsc::Receiver<Command>) -> Self {
        Self {
            rx,
            clients: HashMap::new(),
            tick: 0,
        }
    }

    pub async fn run(mut self) {
        let mut ticker = time::interval(Duration::from_secs(1) / u32::from(TICK_HZ));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            while let Ok(command) = self.rx.try_recv() {
                self.apply(command);
            }
            self.tick += 1;
        }
    }

    fn apply(&mut self, command: Command) {
        match command {
            Command::Join {
                id,
                tx,
                view_h,
                view_v,
            } => {
                let mut frame = BytesMut::new();
                protocol::put_tick(&mut frame, self.tick);
                protocol::put_welcome(&mut frame, id, TICK_HZ);
                // The queue is new and its writer has not written yet, so it
                // is still receiving.
                tx.try_send(frame.freeze())
                    .expect("a new client queue accepts WELCOME");
                self.clients.insert(id, Client { tx, view_h, view_v });
            }
            Command::Leave { id } => {
                self.clients.remove(&id);
            }
            // Entities (§12 step 3) and chunks (step 4).
            Command::Message { .. } => {}
        }
    }
}
