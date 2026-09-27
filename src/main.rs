mod protocol;
mod world;

use std::{
    fmt, io,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
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

/// Entity ids. Never reused while the server runs, not even once exhausted:
/// clients and the world key entities by id, so a reused id could be taken
/// for an entity they still have.
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

/// Why a connection ended.
enum End {
    ClosedByClient,
    /// Also the cause of the close frame, if any (see `close_on_error`).
    Reader(ConnectionError),
    Writer(WebSocketError),
    /// The world disconnected the client, for this reason.
    World(&'static str),
}

impl End {
    /// One line per connection. Leaving, however abruptly, is the client's
    /// doing and goes to stdout; stderr is only for failures on our side.
    fn log(&self, who: impl fmt::Display) {
        match self {
            End::ClosedByClient => println!("{who} disconnected: closed by client"),
            End::World(reason) => println!("{who} disconnected: {reason}"),
            End::Reader(ConnectionError::Protocol(ProtocolError(reason))) => {
                println!("{who} disconnected: protocol error \"{reason}\"");
            }
            End::Reader(ConnectionError::WebSocket(e)) | End::Writer(e) => match e {
                e if is_malformed(e) => println!("{who} disconnected: protocol error \"{e}\""),
                e if is_peer_loss(e) => println!("{who} disconnected: connection lost ({e})"),
                e => eprintln!("{who} failed: {e}"),
            },
        }
    }
}

async fn handle_client(world: mpsc::Sender<Command>, fut: upgrade::UpgradeFut) {
    let ws = match fut.await {
        Ok(ws) => ws,
        Err(e) => return eprintln!("upgrade failed: {e}"),
    };
    // `read_frame` is not cancel-safe, so reading and writing are separate
    // halves instead of a `select!` next to the outbound channel.
    let (read, write) = ws.split(tokio::io::split);
    let mut read = FragmentCollectorRead::new(read);
    let write = Arc::new(Mutex::new(write));

    let Hello { view_xz, view_y } = match read_hello(&mut read, &write).await {
        Ok(Some(hello)) => hello,
        Ok(None) => return End::ClosedByClient.log("new connection"),
        Err(e) => {
            close_on_error(&e, &write).await;
            return End::Reader(e).log("new connection");
        }
    };

    let Ok(id) = NEXT_ID.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
    else {
        eprintln!("entity ids exhausted");
        return close(&write, 1011, "entity ids exhausted").await;
    };
    // Nothing waits for the world: the id is known here, and the reader starts
    // at once. The world sends WELCOME at its next tick, so every server frame,
    // the first included, carries a real tick, and the connection never
    // encodes server messages.
    let (tx, rx) = mpsc::channel(world::OUTBOUND_QUEUE);
    let (disconnect, mut disconnected) = oneshot::channel();
    world
        .send(Command::Join {
            id,
            tx,
            disconnect,
            view_xz,
            view_y,
        })
        .await
        .expect(WORLD_STOPPED);
    let mut writer = tokio::spawn(write_frames(rx, write.clone()));
    println!("client {id} connected (view xz {view_xz}, y {view_y})");

    // This teardown (a stalled client, `disconnected`, the `biased` close) has
    // no automated test. If it changes, check by hand that a client that stops
    // reading, while another one generates traffic, is closed within ~1-2 s.
    //
    // `disconnected` resolves when the world forgets this client: one tick
    // after `Leave`, or when its outbound queue is full. A client that stops
    // reading blocks the writer mid-write, and with it the pongs and the
    // close, so only this signal from the world can end the connection. The
    // reader is abandoned for good, so cancelling `read_frame` here is safe.
    // `biased`: the reader's reason comes first when both are ready.
    let end = select! {
        biased;
        result = read_messages(id, &mut read, &write, &world) => match result {
            Ok(()) => End::ClosedByClient,
            Err(e) => End::Reader(e),
        },
        reason = &mut disconnected => match reason {
            Ok(reason) => End::World(reason),
            // Dropped without a reason: the world found the queue closed,
            // because the writer failed.
            Err(_) => End::Writer(
                (&mut writer)
                    .await
                    .expect("writer panicked")
                    .expect_err("the writer only stops early on an error"),
            ),
        },
    };
    world
        .send(Command::Leave { id })
        .await
        .expect(WORLD_STOPPED);
    if let End::Reader(e) = &end {
        // `biased`: the world may already have forgotten the client, but a
        // writable close must still be attempted first.
        select! {
            biased;
            () = close_on_error(e, &write) => {}
            _ = &mut disconnected => {}
        }
    }
    end.log(format_args!("client {id}"));
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

/// Validates every message once, here. The world trusts the commands.
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
        // Pongs and close replies go through the writer. `read_frame` wraps
        // their errors in `SendError`; unwrapping keeps the cause, often a
        // client that went away.
        let frame = read
            .read_frame(&mut |frame| async move { write.lock().await.write_frame(frame).await })
            .await
            .map_err(|e| match e {
                WebSocketError::SendError(e) => *e
                    .downcast::<WebSocketError>()
                    .expect("the writer's errors are WebSocketError"),
                e => e,
            })?;
        match frame.opcode {
            OpCode::Binary => return Ok(Some(frame.payload)),
            OpCode::Text => return Err(ProtocolError("text messages are not supported").into()),
            OpCode::Close => return Ok(None),
            _ => {}
        }
    }
}

/// Returns when the world closes the queue, or on the first write error.
async fn write_frames<S: AsyncWrite>(
    mut rx: mpsc::Receiver<Bytes>,
    write: Writer<S>,
) -> Result<(), WebSocketError> {
    while let Some(frame) = rx.recv().await {
        let frame = Frame::binary(Payload::Borrowed(&frame));
        write.lock().await.write_frame(frame).await?;
    }
    Ok(())
}

/// Malformed input at the WebSocket layer: the client's fault, like a
/// `ProtocolError`.
fn is_malformed(e: &WebSocketError) -> bool {
    matches!(
        e,
        WebSocketError::InvalidFragment
            | WebSocketError::InvalidUTF8
            | WebSocketError::InvalidContinuationFrame
            | WebSocketError::InvalidCloseFrame
            | WebSocketError::InvalidCloseCode
            | WebSocketError::ReservedBitsNotZero
            | WebSocketError::ControlFrameFragmented
            | WebSocketError::PingFrameTooLarge
            | WebSocketError::FrameTooLarge
            | WebSocketError::InvalidValue
    )
}

/// Closes with 1002 and a reason on malformed input, ours or the WebSocket
/// layer's. Other errors already broke the connection.
async fn close_on_error<S: AsyncWrite>(error: &ConnectionError, write: &Writer<S>) {
    match error {
        ConnectionError::Protocol(ProtocolError(reason)) => close(write, 1002, reason).await,
        // fastwebsockets already answered it with 1002.
        ConnectionError::WebSocket(WebSocketError::InvalidCloseCode) => {}
        ConnectionError::WebSocket(e) if is_malformed(e) => {
            close(write, 1002, &e.to_string()).await;
        }
        ConnectionError::WebSocket(_) => {}
    }
}

/// The client went away: how a connection usually ends without a close.
fn is_peer_loss(e: &WebSocketError) -> bool {
    match e {
        WebSocketError::UnexpectedEOF => true,
        WebSocketError::IoError(e) => matches!(
            e.kind(),
            io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::BrokenPipe
                | io::ErrorKind::UnexpectedEof
        ),
        _ => false,
    }
}

async fn close<S: AsyncWrite>(write: &Writer<S>, code: u16, reason: &str) {
    let frame = Frame::close(code, reason.as_bytes());
    match write.lock().await.write_frame(frame).await {
        Ok(()) => {}
        // The client is gone, and its departure has its own line.
        Err(e) if is_peer_loss(&e) => {}
        Err(e) => eprintln!("close failed: {e}"),
    }
}
