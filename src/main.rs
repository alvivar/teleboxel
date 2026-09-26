mod protocol;
mod world;

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use axum::{Router, extract::State, response::IntoResponse, routing::get};
use bytes::Bytes;
use fastwebsockets::{
    FragmentCollectorRead, Frame, OpCode, Payload, WebSocketError, WebSocketWrite, upgrade,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf},
    select,
    sync::{Mutex, mpsc, oneshot},
};

use protocol::{Hello, ProtocolError};
use world::{Command, World};

type Reader<S> = FragmentCollectorRead<ReadHalf<S>>;
type Writer<S> = Arc<Mutex<WebSocketWrite<WriteHalf<S>>>>;

const WORLD_STOPPED: &str = "world task stopped";

/// Entity ids, never reused while the server runs (§8).
static NEXT_ID: AtomicU32 = AtomicU32::new(0);

#[tokio::main]
async fn main() {
    let (world_tx, world_rx) = mpsc::channel(world::INBOUND_QUEUE);
    tokio::spawn(World::new(world_rx).run());

    let app = Router::new()
        .route("/", get(ws_handler))
        .with_state(world_tx);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn ws_handler(
    State(world): State<mpsc::Sender<Command>>,
    ws: upgrade::IncomingUpgrade,
) -> impl IntoResponse {
    let (response, fut) = ws
        .upgrade()
        .expect("fastwebsockets always builds the response");
    tokio::spawn(handle_client(world, fut));
    response
}

enum ConnectionError {
    Protocol(ProtocolError),
    WebSocket(WebSocketError),
}

impl From<ProtocolError> for ConnectionError {
    fn from(e: ProtocolError) -> Self {
        Self::Protocol(e)
    }
}

impl From<WebSocketError> for ConnectionError {
    fn from(e: WebSocketError) -> Self {
        Self::WebSocket(e)
    }
}

async fn handle_client(world: mpsc::Sender<Command>, fut: upgrade::UpgradeFut) {
    let ws = match fut.await {
        Ok(ws) => ws,
        Err(e) => return eprintln!("upgrade failed: {e}"),
    };
    // `read_frame` is not cancel-safe, so reading and writing are separate
    // halves instead of a `select!` (§8).
    let (read, write) = ws.split(tokio::io::split);
    let mut read = FragmentCollectorRead::new(read);
    let write = Arc::new(Mutex::new(write));

    let Hello { view_h, view_v } = match read_hello(&mut read, &write).await {
        Ok(Some(hello)) => hello,
        Ok(None) => return,
        Err(e) => return close_on_error(e, &write).await,
    };

    let Ok(id) = NEXT_ID.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
    else {
        eprintln!("entity ids exhausted");
        return close(&write, 1011, "entity ids exhausted").await;
    };
    let (tx, rx) = mpsc::channel(world::OUTBOUND_QUEUE);
    let (disconnect, mut disconnected) = oneshot::channel();
    world
        .send(Command::Join {
            id,
            tx,
            disconnect,
            view_h,
            view_v,
        })
        .await
        .expect(WORLD_STOPPED);
    let writer = tokio::spawn(write_frames(rx, write.clone()));

    // `disconnected` resolves when the world forgets this client: one tick
    // after `Leave`, or when it disconnects a slow client (§5.1). That bounds
    // the teardown even when a client that stopped reading keeps the writer,
    // and with it the pongs and the close, blocked. The reader is abandoned
    // for good, so cancelling `read_frame` here is safe.
    let result = select! {
        result = read_messages(id, &mut read, &write, &world) => result,
        _ = &mut disconnected => Ok(()),
    };
    world
        .send(Command::Leave { id })
        .await
        .expect(WORLD_STOPPED);
    if let Err(e) = result {
        // `biased`: the world may already have forgotten the client, but a
        // writable close must still be attempted first.
        select! {
            biased;
            () = close_on_error(e, &write) => {}
            _ = &mut disconnected => {}
        }
    }
    // Dropping the last handles of both halves closes the socket.
    writer.abort();
}

/// Reads the first message. `None` means the client closed before sending it.
async fn read_hello<S: AsyncRead + AsyncWrite>(
    read: &mut Reader<S>,
    write: &Writer<S>,
) -> Result<Option<Hello>, ConnectionError> {
    match next_message(read, write).await? {
        Some(payload) => Ok(Some(protocol::decode_hello(&payload)?)),
        None => Ok(None),
    }
}

async fn read_messages<S: AsyncRead + AsyncWrite>(
    id: u32,
    read: &mut Reader<S>,
    write: &Writer<S>,
    world: &mpsc::Sender<Command>,
) -> Result<(), ConnectionError> {
    while let Some(payload) = next_message(read, write).await? {
        for message in protocol::decode_messages(&payload)? {
            world
                .send(Command::Message { id, message })
                .await
                .expect(WORLD_STOPPED);
        }
    }
    Ok(())
}

/// The payload of the next binary message, or `None` once the client closes.
async fn next_message<S: AsyncRead + AsyncWrite>(
    read: &mut Reader<S>,
    write: &Writer<S>,
) -> Result<Option<Payload<'static>>, ConnectionError> {
    loop {
        // Pongs and close replies go through the writer.
        let frame = read
            .read_frame(&mut |frame| async move { write.lock().await.write_frame(frame).await })
            .await?;
        match frame.opcode {
            OpCode::Binary => return Ok(Some(frame.payload)),
            OpCode::Text => return Err(ProtocolError("text messages are not supported").into()),
            OpCode::Close => return Ok(None),
            _ => {}
        }
    }
}

async fn write_frames<S: AsyncWrite>(mut rx: mpsc::Receiver<Bytes>, write: Writer<S>) {
    while let Some(frame) = rx.recv().await {
        let frame = Frame::binary(Payload::Borrowed(&frame));
        match write.lock().await.write_frame(frame).await {
            Ok(()) => {}
            // A close frame was already written: the connection is ending.
            Err(WebSocketError::ConnectionClosed) => return,
            Err(e) => return eprintln!("write failed: {e}"),
        }
    }
}

/// Closes with 1002 on malformed input (§7.1), ours or the WebSocket layer's.
/// Other errors already broke the connection.
async fn close_on_error<S: AsyncWrite>(error: ConnectionError, write: &Writer<S>) {
    match error {
        ConnectionError::Protocol(ProtocolError(reason)) => close(write, 1002, reason).await,
        ConnectionError::WebSocket(
            e @ (WebSocketError::InvalidFragment
            | WebSocketError::InvalidUTF8
            | WebSocketError::InvalidContinuationFrame
            | WebSocketError::InvalidCloseFrame
            | WebSocketError::ReservedBitsNotZero
            | WebSocketError::ControlFrameFragmented
            | WebSocketError::PingFrameTooLarge
            | WebSocketError::FrameTooLarge
            | WebSocketError::InvalidValue),
        ) => close(write, 1002, &e.to_string()).await,
        // Includes `InvalidCloseCode`, which fastwebsockets already answered with 1002.
        ConnectionError::WebSocket(e) => eprintln!("connection failed: {e}"),
    }
}

async fn close<S: AsyncWrite>(write: &Writer<S>, code: u16, reason: &str) {
    let frame = Frame::close(code, reason.as_bytes());
    if let Err(e) = write.lock().await.write_frame(frame).await {
        eprintln!("close failed: {e}");
    }
}
