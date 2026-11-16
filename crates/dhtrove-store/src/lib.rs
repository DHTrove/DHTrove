//! Crash-recoverable append-only segment store for DHTrove metadata.
//!
//! Each independently checksummed segment entry contains a compressed block of
//! torrent records. Multiple records in a block share compression context.
//! The v1 / v2 infohash index is rebuilt at startup; it is not yet persisted.
//! Segment writes are durable before index entries become visible.
//!
//! This crate is an early storage prototype, not a finished search database.

mod segment;

use dhtrove_storage::{encode_best, Codec, TorrentRecord};
use fs2::FileExt;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use segment::{
    create_segment, read_block, scan_segment, segment_path, FRAME_HEADER_BYTES,
    SEGMENT_HEADER_BYTES,
};

/// Logical segment-data limit; filesystem allocation overhead is not included.
#[derive(Clone, Copy, Debug)]
pub struct StoreConfig {
    pub max_segment_bytes: u64,
    pub max_disk_bytes: Option<u64>,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            max_segment_bytes: 32 * 1024 * 1024,
            max_disk_bytes: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum Infohash {
    V1([u8; 20]),
    V2([u8; 32]),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) struct Location {
    pub segment: u64,
    pub offset: u64,
    pub ordinal: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AppendOutcome {
    pub inserted: usize,
    pub skipped_duplicates: usize,
    pub bytes_written: u64,
    pub codec: Option<Codec>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreStats {
    pub torrent_records: u64,
    pub indexed_hashes: usize,
    pub segments: usize,
    pub disk_bytes: u64,
}

#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Codec(dhtrove_storage::Error),
    Invalid(&'static str),
    Corrupt(String),
    Quota { used: u64, needed: u64, limit: u64 },
    Poisoned,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "storage I/O: {err}"),
            Self::Codec(err) => write!(f, "storage codec: {err}"),
            Self::Invalid(msg) => write!(f, "invalid store configuration or input: {msg}"),
            Self::Corrupt(msg) => write!(f, "corrupt store: {msg}"),
            Self::Quota {
                used,
                needed,
                limit,
            } => write!(
                f,
                "disk quota exceeded: {used} bytes used, {needed} additional, limit {limit}"
            ),
            Self::Poisoned => write!(f, "previous write failed; reopen the store to recover"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Codec(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<dhtrove_storage::Error> for StoreError {
    fn from(err: dhtrove_storage::Error) -> Self {
        Self::Codec(err)
    }
}

/// Exclusive single-writer store. Open the same directory only once per process.
///
/// The index stores two keys for a hybrid torrent without duplicating its
/// encoded block. Existing records are immutable (first writer wins).
pub struct Store {
    root: PathBuf,
    _lock: File,
    writer: File,
    current_segment: u64,
    segments: BTreeMap<u64, u64>,
    index: HashMap<Infohash, Location>,
    record_count: u64,
    disk_bytes: u64,
    config: StoreConfig,
    poisoned: bool,
}

impl Store {
    pub fn open(root: impl AsRef<Path>, config: StoreConfig) -> Result<Self, StoreError> {
        if config.max_segment_bytes < SEGMENT_HEADER_BYTES + FRAME_HEADER_BYTES + 1 {
            return Err(StoreError::Invalid("segment limit is too small"));
        }

        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;

        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join(".dhtrove.lock"))?;
        lock.try_lock_exclusive()?;

        let mut segment_ids = Vec::new();
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            if let Some(id) = segment::parse_segment_name(&entry.file_name().to_string_lossy()) {
                segment_ids.push(id);
            }
        }
        segment_ids.sort_unstable();

        if segment_ids.is_empty() {
            create_segment(&root, 0)?;
            segment_ids.push(0);
        }

        let last_id = *segment_ids
            .last()
            .ok_or(StoreError::Invalid("no active segment"))?;
        let mut segments = BTreeMap::new();
        let mut index = HashMap::new();
        let mut record_count = 0_u64;
        let mut disk_bytes = 0_u64;
        for id in segment_ids {
            let path = segment_path(&root, id);
            let mut file = OpenOptions::new()
                .read(true)
                .write(id == last_id)
                .open(&path)?;
            let length = scan_segment(&mut file, id, id == last_id, &mut index, &mut record_count)?;
            disk_bytes = disk_bytes
                .checked_add(length)
                .ok_or(StoreError::Invalid("disk usage overflow"))?;
            segments.insert(id, length);
        }

        let writer = OpenOptions::new()
            .read(true)
            .write(true)
            .open(segment_path(&root, last_id))?;
        Ok(Self {
            root,
            _lock: lock,
            writer,
            current_segment: last_id,
            segments,
            index,
            record_count,
            disk_bytes,
            config,
            poisoned: false,
        })
    }

    pub fn stats(&self) -> StoreStats {
        StoreStats {
            torrent_records: self.record_count,
            indexed_hashes: self.index.len(),
            segments: self.segments.len(),
            disk_bytes: self.disk_bytes,
        }
    }

    pub fn get(&self, hash: &Infohash) -> Result<Option<TorrentRecord>, StoreError> {
        let Some(location) = self.index.get(hash) else {
            return Ok(None);
        };
        let mut file = File::open(segment_path(&self.root, location.segment))?;
        let block = read_block(&mut file, location.offset)?;
        let record = block
            .into_iter()
            .nth(location.ordinal)
            .ok_or_else(|| StoreError::Corrupt("index references an invalid record".into()))?;
        if !record_keys(&record).contains(hash) {
            return Err(StoreError::Corrupt(
                "infohash does not match index reference".into(),
            ));
        }
        Ok(Some(record))
    }

    /// Append one transaction represented by a single independently recoverable
    /// frame. If any hash already exists, skip that torrent (first writer wins).
    ///
    /// A successful return means sync_data has completed. A failed write poisons
    /// the handle because an incomplete entry may be present at its tail.
    pub fn append(&mut self, records: &[TorrentRecord]) -> Result<AppendOutcome, StoreError> {
        if self.poisoned {
            return Err(StoreError::Poisoned);
        }

        let mut pending = HashSet::new();
        let mut accepted = Vec::new();
        let mut skipped = 0;
        for record in records {
            let keys = record_keys(record);
            if keys.is_empty() {
                return Err(StoreError::Invalid("torrent has no v1 or v2 hash"));
            }
            if keys
                .iter()
                .any(|hash| self.index.contains_key(hash) || pending.contains(hash))
            {
                skipped += 1;
            } else {
                for key in keys {
                    pending.insert(key);
                }
                accepted.push(record.clone());
            }
        }

        if accepted.is_empty() {
            return Ok(AppendOutcome {
                inserted: 0,
                skipped_duplicates: skipped,
                bytes_written: 0,
                codec: None,
            });
        }

        let (codec, encoded) = encode_best(&accepted)?;
        let payload_len = u32::try_from(encoded.len())
            .map_err(|_| StoreError::Invalid("encoded block exceeds u32 length"))?;
        let needed = FRAME_HEADER_BYTES
            .checked_add(u64::from(payload_len))
            .ok_or(StoreError::Invalid("frame length overflow"))?;

        if needed + SEGMENT_HEADER_BYTES > self.config.max_segment_bytes {
            return Err(StoreError::Invalid("block exceeds configured segment size"));
        }

        let active_len = *self
            .segments
            .get(&self.current_segment)
            .ok_or(StoreError::Invalid("active segment not found"))?;
        let rollover = active_len + needed > self.config.max_segment_bytes;
        let growth = needed + if rollover { SEGMENT_HEADER_BYTES } else { 0 };
        if let Some(limit) = self.config.max_disk_bytes {
            if self.disk_bytes > limit || growth > limit - self.disk_bytes {
                return Err(StoreError::Quota {
                    used: self.disk_bytes,
                    needed: growth,
                    limit,
                });
            }
        }

        if rollover {
            let new_id = self
                .current_segment
                .checked_add(1)
                .ok_or(StoreError::Invalid("segment id overflow"))?;
            let file = create_segment(&self.root, new_id)?;
            self.writer = file;
            self.current_segment = new_id;
            self.segments.insert(new_id, SEGMENT_HEADER_BYTES);
            self.disk_bytes += SEGMENT_HEADER_BYTES;
        }

        let offset = *self
            .segments
            .get(&self.current_segment)
            .ok_or(StoreError::Invalid("active segment not found"))?;
        let checksum = crc32fast::hash(&encoded);
        let write = (|| -> std::io::Result<()> {
            self.writer.seek(SeekFrom::Start(offset))?;
            self.writer.write_all(&payload_len.to_le_bytes())?;
            self.writer.write_all(&checksum.to_le_bytes())?;
            self.writer.write_all(&encoded)?;
            self.writer.sync_data()?;
            Ok(())
        })();
        if let Err(err) = write {
            self.poisoned = true;
            return Err(StoreError::Io(err));
        }

        self.disk_bytes += needed;
        *self
            .segments
            .get_mut(&self.current_segment)
            .ok_or(StoreError::Invalid("active segment not found"))? += needed;
        let location = Location {
            segment: self.current_segment,
            offset,
            ordinal: 0,
        };
        for (ordinal, record) in accepted.iter().enumerate() {
            let place = Location {
                ordinal,
                ..location
            };
            for key in record_keys(record) {
                self.index.insert(key, place);
            }
        }
        self.record_count += accepted.len() as u64;
        Ok(AppendOutcome {
            inserted: accepted.len(),
            skipped_duplicates: skipped,
            bytes_written: growth,
            codec: Some(codec),
        })
    }
}

pub(crate) fn record_keys(record: &TorrentRecord) -> Vec<Infohash> {
    let mut keys = Vec::with_capacity(2);
    if let Some(hash) = record.v1 {
        keys.push(Infohash::V1(hash));
    }
    if let Some(hash) = record.v2 {
        keys.push(Infohash::V2(hash));
    }
    keys
}

// The File handle releases the operating system's advisory lock on drop.
impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self._lock);
    }
}
