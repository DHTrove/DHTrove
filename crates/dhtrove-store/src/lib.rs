//! Crash-recoverable append-only segment store for DHTrove metadata.
//!
//! Each independently checksummed segment entry contains a compressed block of
//! torrent records. Multiple records in a block share compression context.
//! Compact Zstd checkpoints accelerate index rebuilding across restarts.
//! Segment writes are durable before index entries become visible.
//!
//! This crate is an early storage prototype, not a finished search database.

mod checkpoint;
mod compaction;
mod manifest;
mod segment;

use dhtrove_storage::{encode_best, Codec, TorrentRecord};
use fs2::FileExt;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use segment::{
    create_segment, encode_tombstone, read_block, remove_live, scan_segment, segment_path,
    FRAME_HEADER_BYTES, SEGMENT_HEADER_BYTES,
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

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
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
    pub checkpoint_bytes: u64,
    pub checkpoint_loaded_on_open: bool,
    pub manifest_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactionOutcome {
    pub performed: bool,
    pub old_bytes: u64,
    pub new_bytes: u64,
    pub reclaimed_bytes: u64,
    pub segments_before: usize,
    pub segments_after: usize,
    pub records_kept: u64,
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
    checkpoint_bytes: u64,
    checkpoint_loaded_on_open: bool,
    manifest_bytes: u64,
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

        // The active manifest, when present, is authoritative. Ignore
        // uncommitted segments left by a crashed compaction.
        manifest::cleanup_staging(&root)?;
        let manifest_ids = manifest::load(&root)?;
        let mut discovered = Vec::new();
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            if let Some(id) = segment::parse_segment_name(&entry.file_name().to_string_lossy()) {
                discovered.push(id);
            }
        }
        discovered.sort_unstable();
        let active_manifest = manifest_ids.is_some();
        let mut segment_ids = manifest_ids.unwrap_or(discovered);

        if segment_ids.is_empty() {
            create_segment(&root, 0)?;
            segment_ids.push(0);
        }

        let last_id = *segment_ids
            .last()
            .ok_or(StoreError::Invalid("no active segment"))?;

        checkpoint::cleanup_temporary(&root)?;
        let mut existing_lengths = BTreeMap::new();
        for &id in &segment_ids {
            existing_lengths.insert(id, fs::metadata(segment_path(&root, id))?.len());
        }
        let loaded = checkpoint::load(&root, &existing_lengths).ok().flatten();
        let checkpoint_loaded_on_open = loaded.is_some();
        let (mut index, mut record_count, saved_lengths) = match loaded {
            Some(snapshot) => (snapshot.index, snapshot.records, snapshot.lengths),
            None => (HashMap::new(), 0, BTreeMap::new()),
        };

        let mut segments = BTreeMap::new();
        let mut disk_bytes = 0_u64;
        for id in segment_ids {
            let path = segment_path(&root, id);
            let mut file = OpenOptions::new()
                .read(true)
                .write(id == last_id)
                .open(&path)?;
            let start_at = saved_lengths
                .get(&id)
                .copied()
                .unwrap_or(SEGMENT_HEADER_BYTES);
            let length = scan_segment(
                &mut file,
                id,
                id == last_id,
                start_at,
                &mut index,
                &mut record_count,
            )?;
            disk_bytes = disk_bytes
                .checked_add(length)
                .ok_or(StoreError::Invalid("disk usage overflow"))?;
            segments.insert(id, length);
        }

        let checkpoint_bytes = if checkpoint_loaded_on_open {
            checkpoint::file_size(&root)?
        } else {
            match fs::remove_file(root.join("hash-index.zst")) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
            0
        };
        let manifest_bytes = manifest::file_size(&root)?;
        disk_bytes = disk_bytes
            .checked_add(checkpoint_bytes)
            .and_then(|bytes| bytes.checked_add(manifest_bytes))
            .ok_or(StoreError::Invalid("disk usage overflow"))?;

        let writer = OpenOptions::new()
            .read(true)
            .write(true)
            .open(segment_path(&root, last_id))?;
        if active_manifest {
            let active: Vec<_> = segments.keys().copied().collect();
            manifest::cleanup_orphans(&root, &active)?;
        }
        Ok(Self {
            root,
            _lock: lock,
            writer,
            current_segment: last_id,
            segments,
            index,
            record_count,
            disk_bytes,
            checkpoint_bytes,
            checkpoint_loaded_on_open,
            manifest_bytes,
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
            checkpoint_bytes: self.checkpoint_bytes,
            checkpoint_loaded_on_open: self.checkpoint_loaded_on_open,
            manifest_bytes: self.manifest_bytes,
        }
    }

    /// Persist a compact, compressed hash index for faster future startups.
    /// The existing checkpoint remains intact if the new one fails.
    pub fn checkpoint(&mut self) -> Result<u64, StoreError> {
        if self.poisoned {
            return Err(StoreError::Poisoned);
        }
        let segments_bytes = self.disk_bytes - self.checkpoint_bytes;
        let size = checkpoint::save(
            &self.root,
            &self.segments,
            &self.index,
            self.record_count,
            segments_bytes,
            self.config.max_disk_bytes,
        )?;
        self.disk_bytes = segments_bytes + size;
        self.checkpoint_bytes = size;
        Ok(size)
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

    /// Rewrite only live records into a fresh generation of segments.
    ///
    /// The manifest switch is atomic: before publication, old segments stay
    /// authoritative; after publication, new segments become authoritative.
    /// Full compaction requires temporary free space roughly equal to the
    /// compacted live dataset. The configured logical quota applies to the
    /// peak usage, not just the final compacted size.
    pub fn compact(&mut self) -> Result<CompactionOutcome, StoreError> {
        if self.poisoned {
            return Err(StoreError::Poisoned);
        }
        let old_bytes = self.disk_bytes;
        let segments_before = self.segments.len();

        // Before creating any new filenames in the data directory, publish an
        // authoritative manifest for existing segments (legacy stores did not
        // have one). Otherwise interrupted moves could cause duplicate replay.
        if self.manifest_bytes == 0 {
            let ids: Vec<u64> = self.segments.keys().copied().collect();
            let additional = manifest::serialized_len(ids.len())?;
            if let Some(limit) = self.config.max_disk_bytes {
                if self.disk_bytes > limit || additional > limit - self.disk_bytes {
                    return Err(StoreError::Quota {
                        used: self.disk_bytes,
                        needed: additional,
                        limit,
                    });
                }
            }
            self.manifest_bytes = manifest::publish(&self.root, &ids)?;
            self.disk_bytes += self.manifest_bytes;
        }

        manifest::cleanup_staging(&self.root)?;
        let staged = match compaction::Staged::build(self) {
            Ok(staged) => staged,
            Err(err) => {
                let _ = manifest::cleanup_staging(&self.root);
                return Err(err);
            }
        };
        let new_ids: Vec<u64> = staged.segments.keys().copied().collect();
        let old_ids: Vec<u64> = self.segments.keys().copied().collect();
        let old_segment_bytes: u64 = self.segments.values().copied().sum();
        if staged.records != self.record_count {
            let _ = manifest::cleanup_staging(&self.root);
            return Err(StoreError::Corrupt(
                "compaction did not preserve live torrent count".into(),
            ));
        }
        // Do not make the database larger merely to rewrite old records.
        if staged.bytes >= old_segment_bytes {
            manifest::cleanup_staging(&self.root)?;
            return Ok(CompactionOutcome {
                performed: false,
                old_bytes,
                new_bytes: self.disk_bytes,
                reclaimed_bytes: 0,
                segments_before,
                segments_after: segments_before,
                records_kept: self.record_count,
            });
        }

        let new_manifest_bytes = manifest::serialized_len(new_ids.len())?;
        if let Some(limit) = self.config.max_disk_bytes {
            let peak = self.disk_bytes.saturating_add(staged.bytes);
            if peak > limit || new_manifest_bytes > limit - peak {
                manifest::cleanup_staging(&self.root)?;
                return Err(StoreError::Quota {
                    used: peak,
                    needed: new_manifest_bytes,
                    limit,
                });
            }
        }

        if let Err(err) = staged.move_into(&self.root) {
            let _ = manifest::cleanup_orphans(&self.root, &old_ids);
            let _ = manifest::cleanup_staging(&self.root);
            return Err(err);
        }
        let new_last = *new_ids
            .last()
            .ok_or(StoreError::Invalid("empty compacted generation"))?;
        let new_writer = match OpenOptions::new()
            .read(true)
            .write(true)
            .open(segment_path(&self.root, new_last))
        {
            Ok(file) => file,
            Err(err) => {
                let _ = manifest::cleanup_orphans(&self.root, &old_ids);
                let _ = manifest::cleanup_staging(&self.root);
                return Err(err.into());
            }
        };

        // No fallible cleanup before the atomic publication point.
        if let Err(err) = manifest::publish(&self.root, &new_ids) {
            // Rename may have completed even when directory fsync fails:
            // do not touch either generation. Reopening will resolve it.
            self.poisoned = true;
            return Err(err);
        }
        self.writer = new_writer;
        self.current_segment = new_last;
        self.segments = staged.segments;
        self.index = staged.index;
        self.record_count = staged.records;
        self.manifest_bytes = new_manifest_bytes;
        self.disk_bytes = staged.bytes + new_manifest_bytes + self.checkpoint_bytes;

        // The previous index checkpoint describes old offsets and must be
        // invalidated; a new one can be explicitly created after compaction.
        let checkpoint_path = self.root.join("hash-index.zst");
        match fs::remove_file(checkpoint_path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                self.poisoned = true;
                return Err(err.into());
            }
        }
        self.disk_bytes -= self.checkpoint_bytes;
        self.checkpoint_bytes = 0;
        self.checkpoint_loaded_on_open = false;

        if let Err(err) = manifest::cleanup_orphans(&self.root, &new_ids) {
            self.poisoned = true;
            return Err(err);
        }
        manifest::cleanup_staging(&self.root)?;
        let new_bytes = self.disk_bytes;
        Ok(CompactionOutcome {
            performed: true,
            old_bytes,
            new_bytes,
            reclaimed_bytes: old_bytes.saturating_sub(new_bytes),
            segments_before,
            segments_after: self.segments.len(),
            records_kept: self.record_count,
        })
    }

    /// Delete a torrent by either of its hashes. A hybrid torrent loses
    /// both lookup keys atomically. The deletion is durable after sync_data,
    /// but space is reclaimed only when segments are compacted.
    pub fn delete(&mut self, hash: &Infohash) -> Result<bool, StoreError> {
        if self.poisoned {
            return Err(StoreError::Poisoned);
        }
        let Some(record) = self.get(hash)? else {
            return Ok(false);
        };
        let keys = record_keys(&record);
        let payload = encode_tombstone(&record)?;
        self.append_payload(&payload)?;
        if let Err(err) = remove_live(&mut self.index, &mut self.record_count, &keys) {
            self.poisoned = true;
            return Err(err);
        }
        Ok(true)
    }

    /// Append and synchronize a generic frame; reused by insert and delete.
    /// Each frame is independent and CRC32 checked when replayed.
    fn append_payload(&mut self, payload: &[u8]) -> Result<(Location, u64), StoreError> {
        let payload_len = u32::try_from(payload.len())
            .map_err(|_| StoreError::Invalid("frame exceeds u32 length"))?;
        let needed = FRAME_HEADER_BYTES
            .checked_add(u64::from(payload_len))
            .ok_or(StoreError::Invalid("frame length overflow"))?;
        if needed + SEGMENT_HEADER_BYTES > self.config.max_segment_bytes {
            return Err(StoreError::Invalid("frame exceeds configured segment size"));
        }

        let active_len = *self
            .segments
            .get(&self.current_segment)
            .ok_or(StoreError::Invalid("active segment not found"))?;
        let rollover = active_len
            .checked_add(needed)
            .ok_or(StoreError::Invalid("segment length overflow"))?
            > self.config.max_segment_bytes;
        let next_manifest_size = if rollover {
            manifest::serialized_len(self.segments.len() + 1)?
        } else {
            self.manifest_bytes
        };
        let manifest_growth = next_manifest_size.saturating_sub(self.manifest_bytes);
        let growth = needed + if rollover { SEGMENT_HEADER_BYTES } else { 0 } + manifest_growth;
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
            let file = match create_segment(&self.root, new_id) {
                Ok(file) => file,
                Err(err) => {
                    self.poisoned = true;
                    return Err(err);
                }
            };
            let mut ids: Vec<u64> = self.segments.keys().copied().collect();
            ids.push(new_id);
            if let Err(err) = manifest::publish(&self.root, &ids) {
                self.poisoned = true;
                return Err(err);
            }
            self.manifest_bytes = next_manifest_size;
            self.disk_bytes += manifest_growth;
            self.writer = file;
            self.current_segment = new_id;
            self.segments.insert(new_id, SEGMENT_HEADER_BYTES);
            self.disk_bytes += SEGMENT_HEADER_BYTES;
        }

        let offset = *self
            .segments
            .get(&self.current_segment)
            .ok_or(StoreError::Invalid("active segment not found"))?;
        let checksum = crc32fast::hash(payload);
        let write = (|| -> std::io::Result<()> {
            self.writer.seek(SeekFrom::Start(offset))?;
            self.writer.write_all(&payload_len.to_le_bytes())?;
            self.writer.write_all(&checksum.to_le_bytes())?;
            self.writer.write_all(payload)?;
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
        Ok((
            Location {
                segment: self.current_segment,
                offset,
                ordinal: 0,
            },
            growth,
        ))
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
        let (location, growth) = self.append_payload(&encoded)?;
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
