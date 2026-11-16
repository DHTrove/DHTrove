//! Atomic active-segment manifest. It is the commit point for compaction.
//!
//! The manifest is introduced lazily: older stores without it treat all
//! segment-*.dhs files as active. Once present, only listed files are read.

use crate::segment::{parse_segment_name, segment_path, sync_directory};
use crate::StoreError;
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
#[cfg(test)]
use std::fs::File;
use std::io::Write;
use std::path::Path;

const MAGIC: &[u8; 4] = b"DHTM";
const VERSION: u8 = 1;
const NAME: &str = "segments.manifest";
const TMP: &str = "segments.manifest.tmp";
const MAX_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn file_size(root: &Path) -> Result<u64, StoreError> {
    match fs::metadata(root.join(NAME)) {
        Ok(info) => Ok(info.len()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn serialized_len(count: usize) -> Result<u64, StoreError> {
    let bytes = 16_usize
        .checked_add(
            count
                .checked_mul(8)
                .ok_or(StoreError::Invalid("too many segment IDs"))?,
        )
        .ok_or(StoreError::Invalid("manifest length overflow"))?;
    if bytes > MAX_BYTES {
        return Err(StoreError::Invalid("manifest exceeds size limit"));
    }
    Ok(bytes as u64)
}

/// Reads a versioned, CRC-protected manifest. A malformed manifest is fatal:
/// guessing active segments here could resurrect deleted data.
pub(crate) fn load(root: &Path) -> Result<Option<Vec<u64>>, StoreError> {
    let bytes = match fs::read(root.join(NAME)) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    if bytes.len() < 16 || bytes.len() > MAX_BYTES {
        return Err(StoreError::Corrupt("invalid manifest length".into()));
    }
    let checksum_offset = bytes.len() - 4;
    let expected = u32::from_le_bytes(
        bytes[checksum_offset..]
            .try_into()
            .map_err(|_| StoreError::Corrupt("manifest checksum truncated".into()))?,
    );
    if crc32fast::hash(&bytes[..checksum_offset]) != expected {
        return Err(StoreError::Corrupt("manifest checksum mismatch".into()));
    }
    if &bytes[..4] != MAGIC || bytes[4] != VERSION || bytes[5..8] != [0, 0, 0] {
        return Err(StoreError::Corrupt("unsupported manifest header".into()));
    }
    let count = u32::from_le_bytes(
        bytes[8..12]
            .try_into()
            .map_err(|_| StoreError::Corrupt("manifest count truncated".into()))?,
    ) as usize;
    if count == 0 || serialized_len(count)? as usize != bytes.len() {
        return Err(StoreError::Corrupt("invalid manifest entry count".into()));
    }
    let mut ids = Vec::with_capacity(count);
    let mut previous = None;
    for chunk in bytes[12..checksum_offset].chunks_exact(8) {
        let id = u64::from_le_bytes(
            chunk
                .try_into()
                .map_err(|_| StoreError::Corrupt("segment ID truncated".into()))?,
        );
        if previous.is_some_and(|last| last >= id) {
            return Err(StoreError::Corrupt("manifest IDs not ascending".into()));
        }
        ids.push(id);
        previous = Some(id);
    }
    for id in &ids {
        if !segment_path(root, *id).is_file() {
            return Err(StoreError::Corrupt("manifest references missing segment".into()));
        }
    }
    Ok(Some(ids))
}

/// Create/replace manifest atomically. The caller must ensure all referenced
/// segments are durable before publication.
pub(crate) fn publish(root: &Path, ids: &[u64]) -> Result<u64, StoreError> {
    if ids.is_empty() || ids.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(StoreError::Invalid("manifest IDs must be strictly ascending"));
    }
    let length = serialized_len(ids.len())?;
    let count = u32::try_from(ids.len())
        .map_err(|_| StoreError::Invalid("too many segment IDs"))?;
    let mut payload = Vec::with_capacity(length as usize);
    payload.extend_from_slice(MAGIC);
    payload.extend_from_slice(&[VERSION, 0, 0, 0]);
    payload.extend_from_slice(&count.to_le_bytes());
    for &id in ids {
        payload.extend_from_slice(&id.to_le_bytes());
    }
    let crc = crc32fast::hash(&payload);
    payload.extend_from_slice(&crc.to_le_bytes());

    let path = root.join(TMP);
    // A stale .tmp is never authoritative.
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&path)?;
        file.write_all(&payload)?;
        file.sync_all()?;
        fs::rename(&path, root.join(NAME))?;
        sync_directory(root)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&path);
    }
    result?;
    Ok(length)
}

/// Only orphan segment files are deleted; never delete an active segment.
pub(crate) fn cleanup_orphans(root: &Path, active: &[u64]) -> Result<(), StoreError> {
    let chosen: BTreeSet<u64> = active.iter().copied().collect();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        if let Some(id) = parse_segment_name(&name.to_string_lossy()) {
            if !chosen.contains(&id) {
                fs::remove_file(entry.path())?;
            }
        }
    }
    sync_directory(root)?;
    Ok(())
}

/// Staging data can be safely removed after acquiring the store writer lock.
/// Only files in this reserved directory can be removed.
pub(crate) fn cleanup_staging(root: &Path) -> Result<(), StoreError> {
    let dir = root.join(".dhtrove-compacting");
    match fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn manifest_round_trip_and_checksum_failure() {
        let dir = tempdir().unwrap();
        File::create(segment_path(dir.path(), 3)).unwrap();
        File::create(segment_path(dir.path(), 5)).unwrap();
        assert!(load(dir.path()).unwrap().is_none());
        assert_eq!(publish(dir.path(), &[3, 5]).unwrap(), 32);
        assert_eq!(load(dir.path()).unwrap(), Some(vec![3, 5]));

        let path = dir.path().join(NAME);
        let mut data = fs::read(&path).unwrap();
        data[15] ^= 1;
        fs::write(path, data).unwrap();
        assert!(matches!(load(dir.path()), Err(StoreError::Corrupt(_))));
    }
}
