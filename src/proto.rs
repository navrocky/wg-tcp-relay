//! Wire protocol between relay client and relay server over a single TCP stream.
//!
//! Handshake (client -> server): `MAGIC(4) | VERSION(1) | session_id(u64 BE)`
//! Handshake (server -> client): `MAGIC(4) | VERSION(1)`
//!
//! Frame: `len(u16 BE) | type(u8) | body`, where `len` counts `type + body`.
//! - DATA:  `peer(u32 BE) | datagram`
//! - PING:  `nonce(u64 BE)`
//! - PONG:  `nonce(u64 BE)`
//! - CLOSE: `peer(u32 BE)` (peer is gone, release its resources)

use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAGIC: &[u8; 4] = b"WGTR";
pub const VERSION: u8 = 1;

const T_DATA: u8 = 0;
const T_PING: u8 = 1;
const T_PONG: u8 = 2;
const T_CLOSE: u8 = 3;

/// Largest datagram that fits into a frame.
pub const MAX_DATAGRAM: usize = u16::MAX as usize - 1 - 4;

#[derive(Debug, PartialEq, Eq)]
pub enum Frame<'a> {
    Data { peer: u32, payload: &'a [u8] },
    Ping(u64),
    Pong(u64),
    Close(u32),
}

pub fn encode_data(peer: u32, payload: &[u8]) -> Vec<u8> {
    debug_assert!(payload.len() <= MAX_DATAGRAM);
    let len = (1 + 4 + payload.len()) as u16;
    let mut v = Vec::with_capacity(2 + len as usize);
    v.extend_from_slice(&len.to_be_bytes());
    v.push(T_DATA);
    v.extend_from_slice(&peer.to_be_bytes());
    v.extend_from_slice(payload);
    v
}

fn encode_u64(t: u8, x: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(11);
    v.extend_from_slice(&9u16.to_be_bytes());
    v.push(t);
    v.extend_from_slice(&x.to_be_bytes());
    v
}

pub fn encode_ping(nonce: u64) -> Vec<u8> {
    encode_u64(T_PING, nonce)
}

pub fn encode_pong(nonce: u64) -> Vec<u8> {
    encode_u64(T_PONG, nonce)
}

pub fn encode_close(peer: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(7);
    v.extend_from_slice(&5u16.to_be_bytes());
    v.push(T_CLOSE);
    v.extend_from_slice(&peer.to_be_bytes());
    v
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Reads one frame into `buf` and returns a view of it.
pub async fn read_frame<'a, R: AsyncRead + Unpin>(
    r: &mut R,
    buf: &'a mut Vec<u8>,
) -> io::Result<Frame<'a>> {
    let len = r.read_u16().await? as usize;
    if len == 0 {
        return Err(invalid("empty frame"));
    }
    buf.resize(len, 0);
    r.read_exact(buf).await?;
    let (t, body) = (buf[0], &buf[1..]);
    let u32_at = |b: &[u8]| u32::from_be_bytes(b[..4].try_into().unwrap());
    let u64_at = |b: &[u8]| u64::from_be_bytes(b[..8].try_into().unwrap());
    match t {
        T_DATA if body.len() >= 4 => Ok(Frame::Data {
            peer: u32_at(body),
            payload: &body[4..],
        }),
        T_PING if body.len() == 8 => Ok(Frame::Ping(u64_at(body))),
        T_PONG if body.len() == 8 => Ok(Frame::Pong(u64_at(body))),
        T_CLOSE if body.len() == 4 => Ok(Frame::Close(u32_at(body))),
        _ => Err(invalid("malformed frame")),
    }
}

pub async fn client_handshake<S: AsyncRead + AsyncWrite + Unpin>(
    s: &mut S,
    session: u64,
) -> io::Result<()> {
    let mut hello = Vec::with_capacity(13);
    hello.extend_from_slice(MAGIC);
    hello.push(VERSION);
    hello.extend_from_slice(&session.to_be_bytes());
    s.write_all(&hello).await?;
    let mut resp = [0u8; 5];
    s.read_exact(&mut resp).await?;
    if &resp[..4] != MAGIC || resp[4] != VERSION {
        return Err(invalid(
            "bad server handshake (not a wg-tcp-relay server or version mismatch)",
        ));
    }
    Ok(())
}

/// Returns the client's session id.
pub async fn server_handshake<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S) -> io::Result<u64> {
    let mut hello = [0u8; 13];
    s.read_exact(&mut hello).await?;
    if &hello[..4] != MAGIC || hello[4] != VERSION {
        return Err(invalid(
            "bad client handshake (not a wg-tcp-relay client or version mismatch)",
        ));
    }
    let mut resp = Vec::with_capacity(5);
    resp.extend_from_slice(MAGIC);
    resp.push(VERSION);
    s.write_all(&resp).await?;
    Ok(u64::from_be_bytes(hello[5..13].try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip() {
        let mut wire = Vec::new();
        wire.extend(encode_data(7, b"hello"));
        wire.extend(encode_ping(42));
        wire.extend(encode_pong(43));
        wire.extend(encode_close(9));
        wire.extend(encode_data(1, b""));
        let mut r = &wire[..];
        let mut buf = Vec::new();
        assert_eq!(
            read_frame(&mut r, &mut buf).await.unwrap(),
            Frame::Data {
                peer: 7,
                payload: b"hello"
            }
        );
        assert_eq!(read_frame(&mut r, &mut buf).await.unwrap(), Frame::Ping(42));
        assert_eq!(read_frame(&mut r, &mut buf).await.unwrap(), Frame::Pong(43));
        assert_eq!(read_frame(&mut r, &mut buf).await.unwrap(), Frame::Close(9));
        assert_eq!(
            read_frame(&mut r, &mut buf).await.unwrap(),
            Frame::Data {
                peer: 1,
                payload: b""
            }
        );
        assert!(read_frame(&mut r, &mut buf).await.is_err()); // EOF
    }

    #[tokio::test]
    async fn rejects_malformed() {
        let mut buf = Vec::new();
        let wire = [0u8, 3, T_PING, 1, 2]; // ping with 2-byte body
        assert!(read_frame(&mut &wire[..], &mut buf).await.is_err());
        let wire = [0u8, 1, 99]; // unknown type
        assert!(read_frame(&mut &wire[..], &mut buf).await.is_err());
    }

    #[tokio::test]
    async fn handshake() {
        let (mut a, mut b) = tokio::io::duplex(64);
        let (c, s) = tokio::join!(
            client_handshake(&mut a, 0xDEADBEEF),
            server_handshake(&mut b)
        );
        c.unwrap();
        assert_eq!(s.unwrap(), 0xDEADBEEF);
    }

    #[tokio::test]
    async fn handshake_rejects_garbage() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
        assert!(server_handshake(&mut b).await.is_err());
    }
}
