//! Segment framing, append-only tombstones, recovery and corruption checks.

use crate::{record_keys, Infohash, Location, StoreError};
use crc32fast::hash;
use dhtrove_storage::{decode_block, TorrentRecord, MAX_BLOCK_BYTES};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 4] = b"DHTS";
const VERSION: u8 = 1;
const TOMBSTONE_MAGIC: &[u8; 4] = b"DHTD";
const TOMBSTONE_VERSION: u8 = 1;
pub(crate) const SEGMENT_HEADER_BYTES: u64 = 8;
pub(crate) const FRAME_HEADER_BYTES: u64 = 8;

pub(crate) enum Frame {
    Records(Vec<TorrentRecord>),
    Tombstone(Vec<Infohash>),
}

pub(crate) fn segment_path(root: &Path, id: u64) -> PathBuf {
    root.join(format!("segment-{id:016x}.dhs"))
}

pub(crate) fn parse_segment_name(name: &str) -> Option<u64> {
    let hex = name.strip_prefix("segment-")?.strip_suffix(".dhs")?;
    if hex.len() != 16 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(hex, 16).ok()
}

pub(crate) fn create_segment(root: &Path, id: u64) -> Result<File, StoreError> {
    let path = segment_path(root, id);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(MAGIC)?;
    file.write_all(&[VERSION, 0, 0, 0])?;
    file.sync_all()?;
    sync_directory(root)?;
    Ok(file)
}

#[cfg(unix)]
pub(crate) fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
pub(crate) fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Encode exactly one logical torrent deletion; a hybrid torrent stores both
/// hashes so replay can invalidate both lookup keys atomically.
pub(crate) fn encode_tombstone(record: &TorrentRecord) -> Result<Vec<u8>, StoreError> {
    if record.v1.is_none() && record.v2.is_none() {
        return Err(StoreError::Invalid("tombstone requires an infohash"));
    }
    let flags = u8::from(record.v1.is_some()) | (u8::from(record.v2.is_some()) << 1);
    let mut bytes = Vec::with_capacity(58);
    bytes.extend_from_slice(TOMBSTONE_MAGIC);
    bytes.push(TOMBSTONE_VERSION);
    bytes.push(flags);
    if let Some(hash) = record.v1 {
        bytes.extend_from_slice(&hash);
    }
    if let Some(hash) = record.v2 {
        bytes.extend_from_slice(&hash);
    }
    Ok(bytes)
}

fn decode_tombstone(payload: &[u8]) -> Result<Vec<Infohash>, StoreError> {
    if payload.len() < 6 || &payload[..4] != TOMBSTONE_MAGIC || payload[4] != TOMBSTONE_VERSION {
        return Err(StoreError::Corrupt("invalid tombstone header".into()));
    }
    let flags = payload[5];
    if flags == 0 || flags & !3 != 0 {
        return Err(StoreError::Corrupt("invalid tombstone flags".into()));
    }
    let expected = 6 + if flags & 1 != 0 { 20 } else { 0 }
        + if flags & 2 != 0 { 32 } else { 0 };
    if payload.len() != expected {
        return Err(StoreError::Corrupt("invalid tombstone length".into()));
    }
    let mut pos = 6;
    let mut keys = Vec::with_capacity(2);
    if flags & 1 != 0 {
        let mut hash = [0; 20];
        hash.copy_from_slice(&payload[pos..pos + 20]);
        keys.push(Infohash::V1(hash));
        pos += 20;
    }
    if flags & 2 != 0 {
        let mut hash = [0; 32];
        hash.copy_from_slice(&payload[pos..pos + 32]);
        keys.push(Infohash::V2(hash));
    }
    Ok(keys)
}

/// A committed tombstone must point to a single existing logical record.
pub(crate) fn remove_live(
    index: &mut HashMap<Infohash, Location>,
    record_count: &mut u64,
    keys: &[Infohash],
) -> Result<(), StoreError> {
    let mut places = keys.iter().map(|key| index.get(key).copied());
    let first = places
        .next()
        .flatten()
        .ok_or_else(|| StoreError::Corrupt("tombstone references a missing torrent".into()))?;
    if places.any(|place| place != Some(first)) {
        return Err(StoreError::Corrupt(
            "tombstone has inconsistent hybrid infohashes".into(),
        ));
    }
    for key in keys {
        index.remove(key);
    }
    *record_count = record_count
        .checked_sub(1)
        .ok_or_else(|| StoreError::Corrupt("tombstone record counter underflow".into()))?;
    Ok(())
}

/// Scan committed frames. Only a partial *last* frame in the active segment
/// may be truncated. Fully present but corrupt frames are never discarded.
pub(crate) fn scan_segment(
    file: &mut File,
    id: u64,
    is_active: bool,
    start_at: u64,
    index: &mut HashMap<Infohash, Location>,
    record_count: &mut u64,
) -> Result<u64, StoreError> {
    let original_len = file.metadata()?.len();
    if original_len < SEGMENT_HEADER_BYTES {
        return Err(StoreError::Corrupt(format!(
            "segment {id} has a truncated header"
        )));
    }
    let mut header = [0u8; SEGMENT_HEADER_BYTES as usize];
    file.read_exact(&mut header)?;
    if &header[..4] != MAGIC || header[4] != VERSION || header[5..] != [0, 0, 0] {
        return Err(StoreError::Corrupt(format!(
            "segment {id} has an invalid header"
        )));
    }
    if start_at < SEGMENT_HEADER_BYTES || start_at > original_len {
        return Err(StoreError::Corrupt(
            "checkpoint scan offset outside segment".into(),
        ));
    }

    let mut pos = start_at;
    while pos < original_len {
        if original_len - pos < FRAME_HEADER_BYTES {
            return recover_tail(file, id, is_active, pos);
        }
        file.seek(SeekFrom::Start(pos))?;
        let mut header = [0u8; FRAME_HEADER_BYTES as usize];
        file.read_exact(&mut header)?;
        let size = u32::from_le_bytes(header[..4].try_into().expect("fixed slice")) as u64;
        if size == 0 || size > MAX_BLOCK_BYTES as u64 {
            return Err(StoreError::Corrupt(format!(
                "segment {id}, offset {pos}: invalid frame length {size}"
            )));
        }
        let end = pos
            .checked_add(FRAME_HEADER_BYTES)
            .and_then(|end| end.checked_add(size))
            .ok_or_else(|| StoreError::Corrupt("segment frame offset overflow".into()))?;
        if end > original_len {
            return recover_tail(file, id, is_active, pos);
        }
        match read_frame(file, pos)? {
            Frame::Records(block) => {
                if block.is_empty() {
                    return Err(StoreError::Corrupt("empty transaction frame".into()));
                }
                for (ordinal, record) in block.iter().enumerate() {
                    let keys = record_keys(record);
                    if keys.is_empty() {
                        return Err(StoreError::Corrupt("record with no infohash".into()));
                    }
                    let location = Location {
                        segment: id,
                        offset: pos,
                        ordinal,
                    };
                    for key in keys {
                        if index.insert(key, location).is_some() {
                            return Err(StoreError::Corrupt(format!(
                                "duplicate infohash found in segment {id}"
                            )));
                        }
                    }
                    *record_count = record_count
                        .checked_add(1)
                        .ok_or_else(|| StoreError::Corrupt("record counter overflow".into()))?;
                }
            }
            Frame::Tombstone(keys) => remove_live(index, record_count, &keys)?,
        }
        pos = end;
    }
    Ok(pos)
}

fn recover_tail(
    file: &mut File,
    id: u64,
    is_active: bool,
    valid_end: u64,
) -> Result<u64, StoreError> {
    if !is_active {
        return Err(StoreError::Corrupt(format!(
            "non-active segment {id} has a partial frame at {valid_end}"
        )));
    }
    file.set_len(valid_end)?;
    file.sync_data()?;
    Ok(valid_end)
}

pub(crate) fn read_frame(file: &mut File, offset: u64) -> Result<Frame, StoreError> {
    file.seek(SeekFrom::Start(offset))?;
    let mut header = [0u8; FRAME_HEADER_BYTES as usize];
    file.read_exact(&mut header)?;
    let size = u32::from_le_bytes(header[..4].try_into().expect("fixed slice")) as usize;
    let checksum = u32::from_le_bytes(header[4..].try_into().expect("fixed slice"));
    if size == 0 || size > MAX_BLOCK_BYTES {
        return Err(StoreError::Corrupt(format!(
            "invalid segment frame length at offset {offset}"
        )));
    }
    let mut bytes = vec![0u8; size];
    file.read_exact(&mut bytes)?;
    if hash(&bytes) != checksum {
        return Err(StoreError::Corrupt(format!(
            "checksum mismatch at offset {offset}"
        )));
    }
    if bytes.starts_with(TOMBSTONE_MAGIC) {
        Ok(Frame::Tombstone(decode_tombstone(&bytes)?))
    } else {
        Ok(Frame::Records(decode_block(&bytes)?))
    }
}

pub(crate) fn read_block(file: &mut File, offset: u64) -> Result<Vec<TorrentRecord>, StoreError> {
    match read_frame(file, offset)? {
        Frame::Records(records) => Ok(records),
        Frame::Tombstone(_) => Err(StoreError::Corrupt(
            "index points to a deletion frame".into(),
        )),
    }
}
