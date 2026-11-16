//! Compact Zstd-compressed hash-index checkpoints.
//!
//! Checkpoints contain key -> frame location references, not torrent metadata.
//! A checkpoint covers an exact prefix of each listed segment. On startup we
//! replay only the committed frames appended after the last checkpoint.

use crate::{Infohash, Location, StoreError};
use dhtrove_storage::MAX_RECORDS;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::Path;

const MAGIC: &[u8; 4] = b"DHTI";
const VERSION: u8 = 1;
const CHECKPOINT: &str = "hash-index.zst";
const TEMPORARY: &str = "hash-index.zst.tmp";

pub(crate) struct Snapshot {
    pub(crate) lengths: BTreeMap<u64, u64>,
    pub(crate) index: HashMap<Infohash, Location>,
    pub(crate) records: u64,
}

pub(crate) fn file_size(root: &Path) -> Result<u64, StoreError> {
    match fs::metadata(root.join(CHECKPOINT)) {
        Ok(metadata) => Ok(metadata.len()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn cleanup_temporary(root: &Path) -> Result<(), StoreError> {
    match fs::remove_file(root.join(TEMPORARY)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Load a checkpoint only if its segment manifest is a valid prefix of the
/// current segment set. Any decode/validation error means callers should
/// rebuild the index by scanning segments.
pub(crate) fn load(
    root: &Path,
    actual: &BTreeMap<u64, u64>,
) -> Result<Option<Snapshot>, StoreError> {
    let file = match File::open(root.join(CHECKPOINT)) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let mut input = zstd::stream::Decoder::new(file)?;
    let mut head = [0u8; 8];
    input.read_exact(&mut head)?;
    if &head[..4] != MAGIC || head[4] != VERSION || head[5..] != [0, 0, 0] {
        return Err(StoreError::Corrupt(
            "invalid hash-index checkpoint header".into(),
        ));
    }

    let segment_count = read_u32(&mut input)? as usize;
    if segment_count == 0 || segment_count > actual.len() {
        return Err(StoreError::Corrupt(
            "checkpoint segment count mismatch".into(),
        ));
    }
    let mut lengths = BTreeMap::new();
    for _ in 0..segment_count {
        let id = read_u64(&mut input)?;
        let size = read_u64(&mut input)?;
        if size < 8 || lengths.insert(id, size).is_some() {
            return Err(StoreError::Corrupt(
                "invalid checkpoint segment manifest".into(),
            ));
        }
    }
    // Exact ordered prefix. Only the last checkpointed segment can grow.
    if !lengths.keys().eq(actual.keys().take(segment_count)) {
        return Err(StoreError::Corrupt(
            "checkpoint segments are not a prefix".into(),
        ));
    }
    let last_id = *lengths
        .last_key_value()
        .ok_or_else(|| StoreError::Corrupt("empty checkpoint manifest".into()))?
        .0;
    for (&id, &saved) in &lengths {
        let current = actual[&id];
        if current < saved || (id != last_id && current != saved) {
            return Err(StoreError::Corrupt(
                "checkpoint segment lengths diverged".into(),
            ));
        }
    }

    let records = read_u64(&mut input)?;
    let key_count = read_u64(&mut input)?;
    if key_count < records || key_count > records.saturating_mul(2) {
        return Err(StoreError::Corrupt("invalid checkpoint key count".into()));
    }

    let mut index = HashMap::new();
    for _ in 0..key_count {
        let mut tag = [0u8; 1];
        input.read_exact(&mut tag)?;
        let key = match tag[0] {
            1 => {
                let mut bytes = [0u8; 20];
                input.read_exact(&mut bytes)?;
                Infohash::V1(bytes)
            }
            2 => {
                let mut bytes = [0u8; 32];
                input.read_exact(&mut bytes)?;
                Infohash::V2(bytes)
            }
            _ => return Err(StoreError::Corrupt("invalid checkpoint key type".into())),
        };
        let segment = read_u64(&mut input)?;
        let offset = read_u64(&mut input)?;
        let ordinal = read_u32(&mut input)? as usize;
        if !lengths
            .get(&segment)
            .is_some_and(|&size| offset >= 8 && offset < size)
            || ordinal >= MAX_RECORDS
        {
            return Err(StoreError::Corrupt("invalid checkpoint location".into()));
        }
        if index
            .insert(
                key,
                Location {
                    segment,
                    offset,
                    ordinal,
                },
            )
            .is_some()
        {
            return Err(StoreError::Corrupt("duplicate checkpoint infohash".into()));
        }
    }
    let mut extra = [0u8; 1];
    if input.read(&mut extra)? != 0 {
        return Err(StoreError::Corrupt("trailing checkpoint data".into()));
    }
    Ok(Some(Snapshot {
        lengths,
        index,
        records,
    }))
}

/// Save an immutable checkpoint using a temporary file, fsync and rename.
/// The reported size is the *final* size after compression, not raw index size.
pub(crate) fn save(
    root: &Path,
    segments: &BTreeMap<u64, u64>,
    index: &HashMap<Infohash, Location>,
    records: u64,
    segments_bytes: u64,
    quota: Option<u64>,
) -> Result<u64, StoreError> {
    cleanup_temporary(root)?;
    let path = root.join(TEMPORARY);

    let result = (|| -> Result<u64, StoreError> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let writer = BufWriter::new(file);
        let mut out = zstd::stream::Encoder::new(writer, 3)?;
        out.include_checksum(true)?;
        out.write_all(MAGIC)?;
        out.write_all(&[VERSION, 0, 0, 0])?;
        let count = u32::try_from(segments.len())
            .map_err(|_| StoreError::Invalid("too many segments for checkpoint"))?;
        out.write_all(&count.to_le_bytes())?;
        for (&id, &size) in segments {
            out.write_all(&id.to_le_bytes())?;
            out.write_all(&size.to_le_bytes())?;
        }
        out.write_all(&records.to_le_bytes())?;
        out.write_all(&(index.len() as u64).to_le_bytes())?;

        // Stable order improves Zstd's ability to compress shared location
        // fields without altering identity or requiring a global dictionary.
        let mut entries: Vec<_> = index.iter().collect();
        entries.sort_unstable_by_key(|(ka, _)| *ka);
        for (key, location) in entries {
            match key {
                Infohash::V1(bytes) => {
                    out.write_all(&[1])?;
                    out.write_all(bytes)?;
                }
                Infohash::V2(bytes) => {
                    out.write_all(&[2])?;
                    out.write_all(bytes)?;
                }
            }
            out.write_all(&location.segment.to_le_bytes())?;
            out.write_all(&location.offset.to_le_bytes())?;
            let ordinal = u32::try_from(location.ordinal)
                .map_err(|_| StoreError::Invalid("checkpoint ordinal exceeds u32"))?;
            out.write_all(&ordinal.to_le_bytes())?;
        }
        let mut writer = out.finish()?;
        writer.flush()?;
        let file = writer
            .into_inner()
            .map_err(|err| StoreError::Io(err.into_error()))?;
        file.sync_all()?;
        let size = file.metadata()?.len();
        let total = segments_bytes
            .checked_add(size)
            .ok_or(StoreError::Invalid("checkpoint disk size overflow"))?;
        if let Some(limit) = quota {
            if total > limit {
                return Err(StoreError::Quota {
                    used: segments_bytes,
                    needed: size,
                    limit,
                });
            }
        }
        fs::rename(&path, root.join(CHECKPOINT))?;
        sync_directory(root)?;
        Ok(size)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&path);
    }
    result
}

fn read_u32(input: &mut impl Read) -> Result<u32, StoreError> {
    let mut buf = [0u8; 4];
    input.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_u64(input: &mut impl Read) -> Result<u64, StoreError> {
    let mut buf = [0u8; 8];
    input.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}
