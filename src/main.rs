use axum::{Router, extract::State, response::IntoResponse, routing::get};
use bytes::Bytes;
use fastwebsockets::{FragmentCollector, Frame, OpCode, Payload, WebSocketError, upgrade};
use std::{collections::HashMap, time::Duration};
use tokio::{
    select,
    sync::{mpsc, oneshot},
    time::MissedTickBehavior,
};

enum WorldMsg {
    Connect {
        reply: oneshot::Sender<PlayerHandShake>,
    },
    Disconnect {
        id: u32,
    },
    SetInterest {
        id: u32,
        center: (i32, i32, i32),
        radius: u16,
    },
    SetPosition {
        id: u32,
        position: (i32, i32, i32),
    },
    SetRotation {
        id: u32,
        rotation: (i32, i32, i32),
    },
}

struct PlayerHandShake {
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
        let tick = Duration::from_secs_f32(1.0 / tick_hz as f32);
        let mut ticker = tokio::time::interval(tick);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            while let Ok(msg) = self.rx.try_recv() {
                self.handle_msg(msg).await;
            }

            // World update logic

            self.broadcast_tick().await;

            ticker.tick().await;
        }
    }

    async fn handle_msg(&mut self, msg: WorldMsg) {
        match msg {
            WorldMsg::Connect { reply } => {
                let id = self.id_count;
                self.id_count += 1;

                let (tx, rx) = mpsc::channel::<Bytes>(128);
                self.players.insert(id, Player { tx, interest: None });

                reply.send(PlayerHandShake { id, rx }).ok();
            }
            WorldMsg::Disconnect { id } => {
                self.players.remove(&id);
            }
            WorldMsg::SetInterest { id, center, radius } => todo!(),
            WorldMsg::SetPosition { id, position } => todo!(),
            WorldMsg::SetRotation { id, rotation } => todo!(),
        }
    }

    async fn broadcast_tick(&mut self) {
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
    let (reply_tx, reply_rx) = oneshot::channel::<PlayerHandShake>();
    handle
        .tx
        .send(WorldMsg::Connect { reply: reply_tx })
        .await
        .ok();
    let PlayerHandShake { id, mut rx } = reply_rx.await.unwrap();

    let mut inner = fut.await?;
    inner.set_auto_close(true);
    inner.set_auto_pong(true);
    inner.set_writev(true);
    let mut ws = FragmentCollector::new(inner);

    let payload = Payload::from(id.to_be_bytes().to_vec());
    let frame = Frame::new(true, OpCode::Binary, None, payload);
    ws.write_frame(frame).await?;

    loop {
        select! {
            frame = ws.read_frame() => {
                if let Ok(frame) = frame {
                    match frame.opcode {
                        OpCode::Close => break,
                        OpCode::Text => {
                            // Maybe we decode the protocol directly here, as
                            // string, for debugging or lazy interactions?
                        },
                        OpCode::Binary => {
                            // We need to decode the type of message and then
                            // send it to the WorldHandle
                        }
                        _ => {}
                    }
                }
                else {
                    // Maybe we should log this?
                    eprintln!("Received unknown frame, not Ok");
                    break;
                }
            }
            Some(bytes) = rx.recv() => {
                let payload = Payload::from(bytes.to_vec());
                let frame = Frame::new(true, OpCode::Binary, None, payload);
                ws.write_frame(frame).await?;
            }
        }
    }

    handle.tx.send(WorldMsg::Disconnect { id }).await.ok();

    Ok(())
}
