//! Wire format, documented for client authors in PROTOCOL.md. The server only
//! decodes client → server messages and only encodes server → client messages.
//! All integers are little-endian.
//!
//! Malformed input is an error, never skipped or guessed: the connection is
//! closed with the error as the reason.

use bytes::{Buf, BufMut, BytesMut, TryGetError};

pub const PROTOCOL_VERSION: u16 = 1;
/// View radii in chunks. They bound each client's tick cost.
pub const MAX_VIEW_XZ: u8 = 16;
pub const MAX_VIEW_Y: u8 = 8;

// Client → server types.
const HELLO: u8 = 0x01;
const ENTITY_STATE_IN: u8 = 0x02;
const VOXEL_EDITS: u8 = 0x03;

// Server → client types.
const WELCOME: u8 = 0x81;
const ENTITY_STATE_OUT: u8 = 0x82;
const ENTITY_REMOVE: u8 = 0x83;
const CHUNK: u8 = 0x84;
const CHUNK_EDITS: u8 = 0x85;
const CHUNK_UNLOAD: u8 = 0x86;

/// Malformed client input. The string is the WebSocket close reason.
#[derive(Debug, PartialEq, Eq)]
pub struct ProtocolError(pub &'static str);

impl From<TryGetError> for ProtocolError {
    fn from(_: TryGetError) -> Self {
        ProtocolError("truncated message")
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Hello {
    pub view_xz: u8,
    pub view_y: u8,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ClientMessage {
    EntityState { pos: [i32; 3], data: Vec<u8> },
    VoxelEdits(Vec<VoxelEdit>),
}

#[derive(Debug, PartialEq, Eq)]
pub struct VoxelEdit {
    pub pos: [i32; 3],
    pub block: u16,
}

/// Decodes the first client message, which must be exactly one `HELLO`.
pub fn decode_hello(mut buf: &[u8]) -> Result<Hello, ProtocolError> {
    if buf.try_get_u8()? != HELLO {
        return Err(ProtocolError("first message must be HELLO"));
    }
    if buf.try_get_u16_le()? != PROTOCOL_VERSION {
        return Err(ProtocolError("unsupported protocol version"));
    }
    let view_xz = buf.try_get_u8()?;
    let view_y = buf.try_get_u8()?;
    if view_xz > MAX_VIEW_XZ {
        return Err(ProtocolError("view_xz too large"));
    }
    if view_y > MAX_VIEW_Y {
        return Err(ProtocolError("view_y too large"));
    }
    if buf.has_remaining() {
        return Err(ProtocolError("HELLO must be the whole message"));
    }
    Ok(Hello { view_xz, view_y })
}

/// Decodes every message of a client message after `HELLO`.
pub fn decode_messages(mut buf: &[u8]) -> Result<Vec<ClientMessage>, ProtocolError> {
    let mut messages = Vec::new();
    while buf.has_remaining() {
        let message = match buf.try_get_u8()? {
            ENTITY_STATE_IN => {
                let pos = get_pos(&mut buf)?;
                let len = buf.try_get_u8()? as usize;
                let (data, rest) = buf
                    .split_at_checked(len)
                    .ok_or(ProtocolError("truncated message"))?;
                buf = rest;
                ClientMessage::EntityState {
                    pos,
                    data: data.to_vec(),
                }
            }
            VOXEL_EDITS => {
                let count = buf.try_get_u16_le()?;
                let mut edits = Vec::new();
                for _ in 0..count {
                    let pos = get_pos(&mut buf)?;
                    let block = buf.try_get_u16_le()?;
                    edits.push(VoxelEdit { pos, block });
                }
                ClientMessage::VoxelEdits(edits)
            }
            _ => return Err(ProtocolError("unexpected message type")),
        };
        messages.push(message);
    }
    Ok(messages)
}

fn get_pos(buf: &mut &[u8]) -> Result<[i32; 3], ProtocolError> {
    Ok([
        buf.try_get_i32_le()?,
        buf.try_get_i32_le()?,
        buf.try_get_i32_le()?,
    ])
}

/// Starts a server → client frame. Clients interpolate entities by this tick,
/// not by arrival time, which carries network jitter.
pub fn put_tick(frame: &mut BytesMut, tick: u32) {
    frame.put_u32_le(tick);
}

pub fn put_welcome(frame: &mut BytesMut, entity_id: u32, tick_hz: u8) {
    frame.put_u8(WELCOME);
    frame.put_u32_le(entity_id);
    frame.put_u8(tick_hz);
}

/// `data` is at most 255 bytes: it arrived with a `u8` length.
pub fn put_entity_state(frame: &mut BytesMut, id: u32, pos: [i32; 3], data: &[u8]) {
    frame.put_u8(ENTITY_STATE_OUT);
    frame.put_u32_le(id);
    put_pos(frame, pos);
    frame.put_u8(data.len() as u8);
    frame.put_slice(data);
}

pub fn put_entity_remove(frame: &mut BytesMut, id: u32) {
    frame.put_u8(ENTITY_REMOVE);
    frame.put_u32_le(id);
}

pub fn put_chunk(frame: &mut BytesMut, pos: [i32; 3], blocks: &[u16; 4096]) {
    frame.put_u8(CHUNK);
    put_pos(frame, pos);
    for &block in blocks {
        frame.put_u16_le(block);
    }
}

/// `edits` are `(index, block)` pairs, at most 2048 of them (`MAX_CHUNK_EDITS`
/// in world.rs), so `count` fits.
pub fn put_chunk_edits(frame: &mut BytesMut, pos: [i32; 3], edits: &[(u16, u16)]) {
    frame.put_u8(CHUNK_EDITS);
    put_pos(frame, pos);
    frame.put_u16_le(edits.len() as u16);
    for &(index, block) in edits {
        frame.put_u16_le(index);
        frame.put_u16_le(block);
    }
}

pub fn put_chunk_unload(frame: &mut BytesMut, pos: [i32; 3]) {
    frame.put_u8(CHUNK_UNLOAD);
    put_pos(frame, pos);
}

fn put_pos(frame: &mut BytesMut, [x, y, z]: [i32; 3]) {
    frame.put_i32_le(x);
    frame.put_i32_le(y);
    frame.put_i32_le(z);
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRUNCATED: ProtocolError = ProtocolError("truncated message");

    #[test]
    fn hello_accepts_valid_and_rejects_malformed() {
        assert_eq!(
            decode_hello(&[0x01, 1, 0, 16, 8]),
            Ok(Hello {
                view_xz: 16,
                view_y: 8
            })
        );
        for len in 0..5 {
            assert_eq!(decode_hello(&[0x01, 1, 0, 16, 8][..len]), Err(TRUNCATED));
        }
        let rejected: [(&[u8], &str); 5] = [
            (&[0x02, 1, 0, 16, 8], "first message must be HELLO"),
            (&[0x01, 2, 0, 16, 8], "unsupported protocol version"),
            (&[0x01, 1, 0, 17, 8], "view_xz too large"),
            (&[0x01, 1, 0, 16, 9], "view_y too large"),
            (
                &[0x01, 1, 0, 16, 8, 0x02],
                "HELLO must be the whole message",
            ),
        ];
        for (buf, reason) in rejected {
            assert_eq!(decode_hello(buf), Err(ProtocolError(reason)));
        }
    }

    #[test]
    fn messages_decode_in_order() {
        #[rustfmt::skip]
        let buf = [
            0x02, 0xff, 0xff, 0xff, 0xff, 2, 0, 0, 0, 0xfd, 0xff, 0xff, 0xff, 2, 0xaa, 0xbb, // ENTITY_STATE
            0x03, 2, 0, // VOXEL_EDITS, count 2
            1, 0, 0, 0, 0xfe, 0xff, 0xff, 0xff, 3, 0, 0, 0, 0x34, 0x12, // edit 1
            0, 0, 0, 0x80, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0x7f, 0, 0, // edit 2
        ];
        assert_eq!(
            decode_messages(&buf),
            Ok(vec![
                ClientMessage::EntityState {
                    pos: [-1, 2, -3],
                    data: vec![0xaa, 0xbb],
                },
                ClientMessage::VoxelEdits(vec![
                    VoxelEdit {
                        pos: [1, -2, 3],
                        block: 0x1234,
                    },
                    VoxelEdit {
                        pos: [i32::MIN, 0, i32::MAX],
                        block: 0,
                    },
                ]),
            ])
        );
    }

    #[test]
    fn messages_reject_malformed() {
        let entity_state: &[u8] = &[0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0xaa, 0xbb];
        let voxel_edits: &[u8] = &[0x03, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x34, 0x12];
        for message in [entity_state, voxel_edits] {
            for len in 1..message.len() {
                assert_eq!(decode_messages(&message[..len]), Err(TRUNCATED));
            }
        }
        for message_type in [0x00, 0x01, 0x04, 0x81] {
            assert_eq!(
                decode_messages(&[message_type]),
                Err(ProtocolError("unexpected message type"))
            );
        }
    }

    #[test]
    fn encoders_produce_exact_bytes() {
        let mut frame = BytesMut::new();
        put_tick(&mut frame, 0x0403_0201);
        put_welcome(&mut frame, 7, 30);
        put_entity_state(&mut frame, 9, [-1, 2, -3], &[0xaa, 0xbb]);
        put_entity_remove(&mut frame, 0x1234_5678);
        put_chunk_edits(&mut frame, [1, -2, 3], &[(0x0102, 0x0304), (4095, 0xffff)]);
        put_chunk_unload(&mut frame, [i32::MIN, 0, i32::MAX]);
        #[rustfmt::skip]
        let expected: &[u8] = &[
            1, 2, 3, 4, // tick
            0x81, 7, 0, 0, 0, 30, // WELCOME
            0x82, 9, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 2, 0, 0, 0, 0xfd, 0xff, 0xff, 0xff, 2, 0xaa, 0xbb, // ENTITY_STATE
            0x83, 0x78, 0x56, 0x34, 0x12, // ENTITY_REMOVE
            0x85, 1, 0, 0, 0, 0xfe, 0xff, 0xff, 0xff, 3, 0, 0, 0, 2, 0, // CHUNK_EDITS, count 2
            0x02, 0x01, 0x04, 0x03, 0xff, 0x0f, 0xff, 0xff, // edits
            0x86, 0, 0, 0, 0x80, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0x7f, // CHUNK_UNLOAD
        ];
        assert_eq!(&frame[..], expected);
    }

    #[test]
    fn chunk_encodes_blocks_in_index_order() {
        let mut blocks = [0; 4096];
        blocks[1] = 0x1234;
        blocks[4095] = 0xabcd;
        let mut frame = BytesMut::new();
        put_chunk(&mut frame, [-1, 2, -3], &blocks);

        let mut expected = vec![
            0x84, 0xff, 0xff, 0xff, 0xff, 2, 0, 0, 0, 0xfd, 0xff, 0xff, 0xff,
        ];
        expected.resize(13 + 8192, 0);
        expected[13 + 2..13 + 4].copy_from_slice(&[0x34, 0x12]);
        expected[13 + 8190..].copy_from_slice(&[0xcd, 0xab]);
        assert_eq!(&frame[..], expected);
    }
}
