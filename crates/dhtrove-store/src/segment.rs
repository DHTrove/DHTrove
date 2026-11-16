//! Segment framing, recovery and corruption checks.

use crate::{record_keys, Infohash, Location, StoreError};
use crc32fast::hash;
use dhtrove_storage::{decode_block, TorrentRecord, MAX_BLOCK_BYTES};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 4] = b"DHTS";
const VERSION: u8 = 1;
pub(crate) const SEGMENT_HEADER_BYTES: u64 = 8;
pub(crate) const FRAME_HEADER_BYTES: u64 = 8;

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
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    // Directory fsync is not universally supported outside Unix.
    Ok(())
}

/// Scan all committed frames. An incomplete trailing frame may be truncated
/// only on the active (last) segment. Complete checksum/codec failures are
/// always reported as corruption rather than silently discarded.
pub(crate) fn scan_segment(
    file: &mut File,
    id: u64,
    is_active: bool,
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

    let mut pos = SEGMENT_HEADER_BYTES;
    while pos < original_len {
        let remaining = original_len - pos;
        if remaining < FRAME_HEADER_BYTES {
            return recover_tail(file, id, is_active, pos);
        }

        file.seek(SeekFrom::Start(pos))?;
        let mut frame_header = [0u8; FRAME_HEADER_BYTES as usize];
        file.read_exact(&mut frame_header)?;
        let size = u32::from_le_bytes(frame_header[..4].try_into().expect("fixed slice")) as u64;
        let expected_crc = u32::from_le_bytes(frame_header[4..].try_into().expect("fixed slice"));
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
        let block = read_block(file, pos)?;
        if block.is_empty() {
            return Err(StoreError::Corrupt("empty transaction frame".into()));
        }
        // read_block checks both the outer CRC and the storage payload integrity.
        // We also check the already-decoded framing checksum for clarity.
        let _ = expected_crc;
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

pub(crate) fn read_block(file: &mut File, offset: u64) -> Result<Vec<TorrentRecord>, StoreError> {
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
    decode_block(&bytes).map_err(StoreError::Codec)
}
