//! Outbound metadata-only BitTorrent TCP exchange (BEP 9/10).
//!
//! Never requests torrent payload pieces, and never downloads a .torrent file.
//! A peer's advertised metadata is bounded and validated against the original
//! 20-byte DHT hash (SHA-1 or SHA-256 truncated for v2).

use crate::info::{decode_info, Error as InfoError};
use dhtrove_dht::bencode::{self, Value};
use dhtrove_storage::TorrentRecord;
use std::net::SocketAddrV4;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

const PSTR: &[u8] = b"BitTorrent protocol";
const PIECE: usize = 16 * 1024;
const OUR_METADATA_ID: u8 = 1;

#[derive(Clone, Copy, Debug)]
pub struct FetchConfig {
    pub connect_timeout: Duration,
    pub transfer_timeout: Duration,
    pub max_metadata_bytes: usize,
    pub max_frame_bytes: usize,
    pub pipeline: usize,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            transfer_timeout: Duration::from_secs(25),
            max_metadata_bytes: 4 * 1024 * 1024,
            max_frame_bytes: 4 * 1024 * 1024 + 2048,
            pipeline: 4,
        }
    }
}

#[derive(Debug)]
pub enum FetchError {
    Io(std::io::Error),
    Timeout,
    Protocol(&'static str),
    Info(InfoError),
}
impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "peer metadata fetch: {self:?}")
    }
}
impl std::error::Error for FetchError {}
impl From<std::io::Error> for FetchError {
    fn from(err: std::io::Error) -> Self { Self::Io(err) }
}
impl From<InfoError> for FetchError {
    fn from(err: InfoError) -> Self { Self::Info(err) }
}

pub async fn fetch(
    peer: SocketAddrV4,
    observed_hash: [u8; 20],
    peer_id: [u8; 20],
    config: FetchConfig,
) -> Result<TorrentRecord, FetchError> {
    let stream = timeout(config.connect_timeout, TcpStream::connect(peer))
        .await.map_err(|_| FetchError::Timeout)??;
    timeout(config.transfer_timeout, exchange(stream, observed_hash, peer_id, config))
        .await.map_err(|_| FetchError::Timeout)?
}

async fn exchange(
    mut stream: TcpStream,
    target: [u8; 20],
    peer_id: [u8; 20],
    config: FetchConfig,
) -> Result<TorrentRecord, FetchError> {
    let mut handshake = [0u8; 68];
    handshake[0] = 19;
    handshake[1..20].copy_from_slice(PSTR);
    handshake[25] = 0x10; // reserved[5]: BEP 10 extended messages.
    handshake[28..48].copy_from_slice(&target);
    handshake[48..68].copy_from_slice(&peer_id);
    stream.write_all(&handshake).await?;

    let mut reply = [0u8; 68];
    stream.read_exact(&mut reply).await?;
    if reply[0] != 19 || &reply[1..20] != PSTR || reply[28..48] != target {
        return Err(FetchError::Protocol("peer handshake mismatch"));
    }
    if reply[25] & 0x10 == 0 {
        return Err(FetchError::Protocol("peer does not support BEP 10"));
    }

    let extension_handshake = bencode::encode(&Value::Dict(vec![
        (b"m", Value::Dict(vec![(b"ut_metadata", Value::Integer(i64::from(OUR_METADATA_ID)))])),
    ]));
    let mut msg = vec![20, 0];
    msg.extend_from_slice(&extension_handshake);
    write_frame(&mut stream, &msg).await?;

    let mut their_metadata_id = None;
    let mut metadata_size = None;
    for _ in 0..100 {
        let packet = read_frame(&mut stream, config.max_frame_bytes).await?;
        if packet.len() < 2 || packet[0] != 20 || packet[1] != 0 { continue; }
        let header = bencode::decode(&packet[2..], config.max_frame_bytes)
            .map_err(|_| FetchError::Protocol("invalid BEP 10 handshake"))?;
        let id = header.get(b"m")
            .and_then(|v| v.get(b"ut_metadata"))
            .and_then(Value::integer)
            .and_then(|v| u8::try_from(v).ok())
            .filter(|id| *id != 0)
            .ok_or(FetchError::Protocol("peer does not advertise ut_metadata"))?;
        let size = header.get(b"metadata_size").and_then(Value::integer)
            .and_then(|v| usize::try_from(v).ok())
            .filter(|size| *size > 0 && *size <= config.max_metadata_bytes)
            .ok_or(FetchError::Protocol("invalid or oversized metadata size"))?;
        their_metadata_id = Some(id);
        metadata_size = Some(size);
        break;
    }
    let remote_id = their_metadata_id.ok_or(FetchError::Protocol("missing extension handshake"))?;
    let total = metadata_size.ok_or(FetchError::Protocol("missing metadata size"))?;
    let pieces = total.div_ceil(PIECE);
    let mut chunks: Vec<Option<Vec<u8>>> = vec![None; pieces];
    let mut requested = vec![false; pieces];
    let mut sent = 0usize;
    let mut completed = 0usize;
    let max_window = config.pipeline.clamp(1, 16);
    let mut inflight = 0usize;

    while completed < pieces {
        while sent < pieces && inflight < max_window {
            let req = bencode::encode(&Value::Dict(vec![
                (b"msg_type", Value::Integer(0)),
                (b"piece", Value::Integer(sent as i64)),
            ]));
            let mut message = vec![20, remote_id];
            message.extend_from_slice(&req);
            write_frame(&mut stream, &message).await?;
            requested[sent] = true;
            sent += 1;
            inflight += 1;
        }
        let response = read_frame(&mut stream, config.max_frame_bytes).await?;
        if response.len() < 2 || response[0] != 20 || response[1] != OUR_METADATA_ID {
            continue; // ignore non-metadata and unknown extension messages.
        }
        let (header, prefix) = bencode::decode_prefix(&response[2..], config.max_frame_bytes)
            .map_err(|_| FetchError::Protocol("malformed BEP 9 response header"))?;
        let kind = header.get(b"msg_type").and_then(Value::integer)
            .ok_or(FetchError::Protocol("missing metadata message type"))?;
        let piece = header.get(b"piece").and_then(Value::integer)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or(FetchError::Protocol("invalid metadata piece index"))?;
        if piece >= pieces || !requested[piece] {
            return Err(FetchError::Protocol("unsolicited metadata piece"));
        }
        if kind == 2 { return Err(FetchError::Protocol("peer rejected metadata request")); }
        if kind != 1 { continue; }
        let advertised = header.get(b"total_size").and_then(Value::integer)
            .and_then(|v| usize::try_from(v).ok());
        if advertised != Some(total) {
            return Err(FetchError::Protocol("metadata size changed mid-transfer"));
        }
        let payload = &response[2 + prefix..];
        let expected_len = (total - piece * PIECE).min(PIECE);
        if payload.len() != expected_len {
            return Err(FetchError::Protocol("invalid metadata piece length"));
        }
        if chunks[piece].is_none() {
            chunks[piece] = Some(payload.to_vec());
            completed += 1;
            inflight -= 1;
        }
    }
    let mut raw = Vec::with_capacity(total);
    for chunk in chunks {
        raw.extend_from_slice(&chunk.ok_or(FetchError::Protocol("missing metadata chunk"))?);
    }
    decode_info(&raw, &target).map_err(FetchError::Info)
}

async fn write_frame(stream: &mut TcpStream, bytes: &[u8]) -> Result<(), FetchError> {
    let length = u32::try_from(bytes.len()).map_err(|_| FetchError::Protocol("outgoing frame too large"))?;
    stream.write_all(&length.to_be_bytes()).await?;
    stream.write_all(bytes).await?;
    Ok(())
}

async fn read_frame(stream: &mut TcpStream, max: usize) -> Result<Vec<u8>, FetchError> {
    let length = stream.read_u32().await? as usize;
    if length > max { return Err(FetchError::Protocol("peer frame exceeds metadata limit")); }
    let mut data = vec![0; length];
    stream.read_exact(&mut data).await?;
    Ok(data)
}
