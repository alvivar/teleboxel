//! Load bot: N well-behaved clients against a running server (SPECIFICATION.md
//! §12 step 6), to measure §11 with the server's stats.
//!
//! Usage: bot [clients=50] [addr=127.0.0.1:3000] [seconds=30] [view_h=8]
//!            [view_v=4] [edits_per_sec=2]
//!
//! Each bot walks a circle that crosses chunk borders, sends its entity every
//! server tick, edits the voxel under it at the given rate, and reads every
//! frame. Every 5 s it prints what the bots received, to cross-check the
//! server's "out" figure.

use std::{
    env,
    error::Error,
    process,
    str::FromStr,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use bytes::{BufMut, BytesMut};
use fastwebsockets::{Frame, OpCode, Payload, Role, WebSocket, WebSocketError};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{self, MissedTickBehavior},
};

type BoxError = Box<dyn Error + Send + Sync>;

const REPORT_PERIOD: Duration = Duration::from_secs(5);
/// Bots walk circles of this radius (voxels) around grid points this far apart,
/// so each one crosses chunk borders and sees its neighbors.
const RADIUS: f64 = 24.0;
const SPACING: f64 = 48.0;
const SPEED: f64 = 6.0; // voxels per second
/// Opaque entity data (§3.2), sized like orientation + animation state.
const ENTITY_DATA: [u8; 8] = [0; 8];

static CONNECTED: AtomicUsize = AtomicUsize::new(0);
static FAILED: AtomicUsize = AtomicUsize::new(0);
static FRAMES: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

struct Config {
    clients: usize,
    addr: String,
    duration: Duration,
    view_h: u8,
    view_v: u8,
    edits_per_sec: f64,
}

#[tokio::main]
async fn main() {
    let config: &'static Config = Box::leak(Box::new(parse_args()));
    let bots: Vec<_> = (0..config.clients)
        .map(|index| {
            tokio::spawn(async move {
                if let Err(e) = run_bot(index, config).await {
                    FAILED.fetch_add(1, Ordering::Relaxed);
                    eprintln!("bot {index}: {e}");
                }
            })
        })
        .collect();

    let start = Instant::now();
    let mut report = time::interval_at(time::Instant::now() + REPORT_PERIOD, REPORT_PERIOD);
    report.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let (mut last, mut frames, mut bytes) = (start, 0, 0);
    while start.elapsed() + REPORT_PERIOD <= config.duration {
        report.tick().await;
        let (total_frames, total_bytes) = (
            FRAMES.load(Ordering::Relaxed),
            BYTES.load(Ordering::Relaxed),
        );
        // The actual window: a report can be late under load.
        let secs = last.elapsed().as_secs_f64();
        last = Instant::now();
        println!(
            "connected {} | received {:.1} KiB/s, {:.0} frames/s | failed {}",
            CONNECTED.load(Ordering::Relaxed),
            (total_bytes - bytes) as f64 / 1024.0 / secs,
            (total_frames - frames) as f64 / secs,
            FAILED.load(Ordering::Relaxed),
        );
        (frames, bytes) = (total_frames, total_bytes);
    }
    for bot in bots {
        bot.await.expect("bot task panicked");
    }
    let secs = start.elapsed().as_secs_f64();
    println!(
        "done: {} bots, {} failed | {} frames, {:.1} MiB in {secs:.0} s ({:.1} KiB/s)",
        config.clients,
        FAILED.load(Ordering::Relaxed),
        FRAMES.load(Ordering::Relaxed),
        BYTES.load(Ordering::Relaxed) as f64 / 1024.0 / 1024.0,
        BYTES.load(Ordering::Relaxed) as f64 / 1024.0 / secs,
    );
}

fn parse_args() -> Config {
    let args: Vec<String> = env::args().skip(1).collect();
    let config = Config {
        clients: arg(&args, 0, "clients", 50),
        addr: arg(&args, 1, "addr", "127.0.0.1:3000".to_string()),
        duration: Duration::from_secs(arg(&args, 2, "seconds", 30)),
        view_h: arg(&args, 3, "view_h", 8),
        view_v: arg(&args, 4, "view_v", 4),
        edits_per_sec: arg(&args, 5, "edits_per_sec", 2.0),
    };
    if !(config.edits_per_sec.is_finite() && config.edits_per_sec >= 0.0) {
        usage_error("edits_per_sec", &args[5]);
    }
    config
}

fn arg<T: FromStr>(args: &[String], index: usize, name: &str, default: T) -> T {
    match args.get(index) {
        None => default,
        Some(value) => value.parse().unwrap_or_else(|_| usage_error(name, value)),
    }
}

fn usage_error(name: &str, value: &str) -> ! {
    eprintln!("invalid {name}: {value}");
    eprintln!(
        "usage: bot [clients=50] [addr=127.0.0.1:3000] [seconds=30] [view_h=8] [view_v=4] [edits_per_sec=2]"
    );
    process::exit(2);
}

async fn run_bot(index: usize, config: &Config) -> Result<(), BoxError> {
    let mut stream = TcpStream::connect(&config.addr).await?;
    handshake(&mut stream, &config.addr).await?;
    let mut ws = WebSocket::after_handshake(stream, Role::Client);
    // The server sends no pings, and its close is either the reply to ours or
    // a failure to report.
    ws.set_auto_close(false);
    ws.set_auto_pong(false);

    ws.write_frame(binary(hello(config))).await?;
    let welcome = ws.read_frame().await?;
    let tick_hz = parse_welcome(&welcome)?;
    count(&welcome);
    // Keeps each tick's edit count within VOXEL_EDITS' u16 `count`.
    if config.edits_per_sec / f64::from(tick_hz) >= f64::from(u16::MAX) {
        return Err(format!("edits_per_sec above {} per tick at {tick_hz} Hz", u16::MAX).into());
    }

    let (mut read, mut write) = ws.split(|stream| stream.into_split());
    CONNECTED.fetch_add(1, Ordering::Relaxed);
    let closing = AtomicBool::new(false);
    let reading = async {
        loop {
            let frame = read
                .read_frame(&mut |_| async { Ok::<_, WebSocketError>(()) })
                .await?;
            match frame.opcode {
                OpCode::Binary => count(&frame),
                OpCode::Close if closing.load(Ordering::Relaxed) => return Ok(()),
                OpCode::Close => {
                    return Err(format!("closed by the server: {}", close_reason(&frame)).into());
                }
                opcode => return Err(format!("unexpected {opcode:?} frame").into()),
            }
        }
    };
    let writing = async {
        let mut ticker = time::interval(Duration::from_secs(1) / u32::from(tick_hz));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let start = Instant::now();
        let mut edits_due = 0.0;
        while start.elapsed() < config.duration {
            ticker.tick().await;
            let pos = position(index, start.elapsed().as_secs_f64());
            write.write_frame(binary(entity_state(pos))).await?;

            edits_due += config.edits_per_sec / f64::from(tick_hz);
            let edits = edits_due.floor();
            if edits > 0.0 {
                edits_due -= edits;
                write
                    .write_frame(binary(voxel_edits(pos, edits as u16)))
                    .await?;
            }
        }
        closing.store(true, Ordering::Relaxed);
        write.write_frame(Frame::close(1000, b"")).await?;
        Ok::<_, BoxError>(())
    };
    // Done when our close is answered. The first failure on either side ends
    // the bot; the other side is never polled again, so cancelling it is safe.
    let result = tokio::try_join!(reading, writing).map(|_| ());
    CONNECTED.fetch_sub(1, Ordering::Relaxed);
    result
}

/// Minimal client upgrade. The key is RFC 6455's sample nonce: the server
/// only answers it, so a fixed one is enough for a load tool.
async fn handshake(stream: &mut TcpStream, addr: &str) -> Result<(), BoxError> {
    let request = format!(
        "GET / HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    // One byte at a time, so nothing after the headers is consumed here.
    let mut response = Vec::new();
    while !response.ends_with(b"\r\n\r\n") {
        response.push(stream.read_u8().await?);
    }
    if !response.starts_with(b"HTTP/1.1 101 ") {
        let response = String::from_utf8_lossy(&response);
        return Err(format!("upgrade refused: {response}").into());
    }
    Ok(())
}

fn count(frame: &Frame) {
    FRAMES.fetch_add(1, Ordering::Relaxed);
    BYTES.fetch_add(frame.payload.len() as u64, Ordering::Relaxed);
}

fn binary(message: BytesMut) -> Frame<'static> {
    Frame::binary(Payload::Bytes(message))
}

/// §7.3: `u32 tick, 0x81 WELCOME, u32 entity_id, u8 tick_hz`.
fn parse_welcome(frame: &Frame) -> Result<u8, BoxError> {
    match (frame.opcode, &frame.payload[..]) {
        (OpCode::Binary, [_, _, _, _, 0x81, _, _, _, _, tick_hz]) => Ok(*tick_hz),
        (OpCode::Close, _) => Err(format!("closed by the server: {}", close_reason(frame)).into()),
        _ => Err(format!(
            "expected WELCOME, got {:?} {:?}",
            frame.opcode,
            &frame.payload[..]
        )
        .into()),
    }
}

fn close_reason(frame: &Frame) -> String {
    match &frame.payload[..] {
        [a, b, reason @ ..] => {
            let code = u16::from_be_bytes([*a, *b]);
            format!("{code} {}", String::from_utf8_lossy(reason))
        }
        _ => "no code".to_string(),
    }
}

/// Voxel position of bot `index` after `secs` seconds.
fn position(index: usize, secs: f64) -> [f64; 3] {
    let center = [
        (index % 8) as f64 * SPACING,
        0.0,
        (index / 8) as f64 * SPACING,
    ];
    let angle = index as f64 + secs * SPEED / RADIUS;
    [
        center[0] + RADIUS * angle.cos(),
        center[1] + 1.5,
        center[2] + RADIUS * angle.sin(),
    ]
}

// Client → server messages (§7.2).

fn hello(config: &Config) -> BytesMut {
    let mut message = BytesMut::new();
    message.put_u8(0x01);
    message.put_u16_le(1); // PROTOCOL_VERSION
    message.put_u8(config.view_h);
    message.put_u8(config.view_v);
    message
}

fn entity_state(pos: [f64; 3]) -> BytesMut {
    let mut message = BytesMut::new();
    message.put_u8(0x02);
    for coordinate in pos {
        message.put_i32_le((coordinate * 256.0) as i32); // 24.8 fixed point (§4)
    }
    message.put_u8(ENTITY_DATA.len() as u8);
    message.put_slice(&ENTITY_DATA);
    message
}

/// `count` voxels in a row on the ground under `pos`.
fn voxel_edits(pos: [f64; 3], count: u16) -> BytesMut {
    let [x, _, z] = pos.map(|c| c.floor() as i32);
    let mut message = BytesMut::new();
    message.put_u8(0x03);
    message.put_u16_le(count);
    for i in 0..i32::from(count) {
        message.put_i32_le(x + i);
        message.put_i32_le(0);
        message.put_i32_le(z);
        message.put_u16_le(1 + (x + z + i).rem_euclid(255) as u16);
    }
    message
}
