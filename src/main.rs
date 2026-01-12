use axum::{Router, extract::State, response::IntoResponse, routing::get};
use bytes::{Bytes, BytesMut};
use fastwebsockets::{FragmentCollector, Frame, OpCode, Payload, WebSocketError, upgrade};
use std::{collections::HashMap, time::Duration};
use tokio::{
    select,
    sync::{mpsc, oneshot},
    time::MissedTickBehavior,
};

enum WorldMsg {
    Connect {
        reply: oneshot::Sender<PlayerHandshake>,
    },
    Disconnect {
        id: u32,
    },
    SetInterest {
        id: u32,
        center: (i32, i32, i32),
        radius: u16,
    },
}

struct PlayerHandshake {
    id: u32,
    rx: mpsc::Receiver<Bytes>,
}

struct Player {
    tx: mpsc::Sender<Bytes>,
    interest: Option<((i32, i32, i32), u16)>,
}

#[derive(Clone)]
struct WorldHandle {
    tx: mpsc::Sender<WorldMsg>,
}

struct World {
    id_count: u32,
    rx: mpsc::Receiver<WorldMsg>,
    players: HashMap<u32, Player>,
}

impl World {
    fn new(rx: mpsc::Receiver<WorldMsg>) -> Self {
        Self {
            id_count: 1,
            rx,
            players: HashMap::new(),
        }
    }

    async fn run(mut self, tick_hz: u32) {
        // Avoid float math + rounding drift
        let tick = Duration::from_nanos(1_000_000_000u64 / tick_hz as u64);
        let mut ticker = tokio::time::interval(tick);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            select! {
                // Tick path: drain any queued messages, then update+broadcast once
                _ = ticker.tick() => {
                    while let Ok(msg) = self.rx.try_recv() {
                        self.handle_msg(msg);
                    }

                    // World update logic
                    self.broadcast_tick();
                }

                // Low-latency path: process messages as they arrive
                Some(msg) = self.rx.recv() => {
                    self.handle_msg(msg);
                }

                // Channel closed => shut down world task
                else => break,
            }
        }
    }

    fn handle_msg(&mut self, msg: WorldMsg) {
        match msg {
            WorldMsg::Connect { reply } => {
                let id = self.id_count;
                self.id_count += 1;

                let (tx, rx) = mpsc::channel::<Bytes>(128);
                self.players.insert(id, Player { tx, interest: None });

                reply.send(PlayerHandshake { id, rx }).ok();
            }
            WorldMsg::Disconnect { id } => {
                self.players.remove(&id);
            }
            WorldMsg::SetInterest { id, center, radius } => {
                if let Some(player) = self.players.get_mut(&id) {
                    player.interest = Some((center, radius));
                }
            }
        }
    }

    fn broadcast_tick(&mut self) {
        for (id, player) in self.players.iter_mut() {
            if player.interest.is_none() {
                continue;
            }

            // We should filter by area of interest, then send
        }
    }
}

#[tokio::main]
async fn main() {
    let (tx, rx) = mpsc::channel::<WorldMsg>(128);
    let world = World::new(rx);
    tokio::spawn(world.run(60));

    let handle = WorldHandle { tx };
    let app = Router::new().route("/", get(ws_handler)).with_state(handle);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn ws_handler(
    State(handle): State<WorldHandle>,
    ws: upgrade::IncomingUpgrade,
) -> impl IntoResponse {
    let (response, fut) = ws.upgrade().unwrap();
    tokio::task::spawn(async move {
        if let Err(e) = handle_client(handle, fut).await {
            eprintln!("Error handling client: {}", e);
        }
    });

    response
}

async fn handle_client(
    handle: WorldHandle,
    fut: upgrade::UpgradeFut,
) -> Result<(), WebSocketError> {
    let (reply_tx, reply_rx) = oneshot::channel::<PlayerHandshake>();
    handle
        .tx
        .send(WorldMsg::Connect { reply: reply_tx })
        .await
        .ok();
    let PlayerHandshake { id, mut rx } = reply_rx.await.unwrap();

    let mut inner = fut.await?;
    inner.set_auto_close(true);
    inner.set_auto_pong(true);
    inner.set_writev(true);
    let mut ws = FragmentCollector::new(inner);

    let handshake_id = id.to_string();
    let frame = Frame::text(Payload::from(handshake_id.as_bytes()));
    ws.write_frame(frame).await?;

    loop {
        select! {
            frame = ws.read_frame() => {
                if let Ok(frame) = frame {
                    match frame.opcode {
                        OpCode::Close => break,
                        OpCode::Text => {
                            let parts: Vec<&str> = str::from_utf8(&frame.payload)
                                .unwrap_or("")
                                .split(' ')
                                .collect();

                            // SetInterest PosX PosY PosZ Radius

                            if parts[0] == "SetInterest" {
                                if parts.len() != 5 {
                                    let payload = Payload::from(b"SetInterest Invalid" as &[u8]);
                                    ws.write_frame(Frame::text(payload)).await?;
                                    continue;
                                }

                                let center = (
                                    parts[1].parse::<i32>().unwrap(),
                                    parts[2].parse::<i32>().unwrap(),
                                    parts[3].parse::<i32>().unwrap(),
                                );
                                let radius = parts[4].parse::<u16>().unwrap();

                                handle
                                    .tx
                                    .send(WorldMsg::SetInterest { id, center, radius })
                                    .await
                                    .ok();

                                let payload = Payload::from(b"SetInterest Ok" as &[u8]);
                                ws.write_frame(Frame::text(payload)).await?;
                            }
                        }
                        OpCode::Binary => {
                            // Eventually, we need to translate the Text
                            // protocol to binary
                        }
                        _ => {}
                    }
                } else {
                    // Maybe we should log this?
                    eprintln!("Received unknown frame, not Ok");
                    break;
                }
            }
            Some(bytes) = rx.recv() => {
                let payload = Payload::Bytes(BytesMut::from(bytes));
                ws.write_frame(Frame::binary(payload)).await?;
            }
        }
    }

    handle.tx.send(WorldMsg::Disconnect { id }).await.ok();

    Ok(())
}
