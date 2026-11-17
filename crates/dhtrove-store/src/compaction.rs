//! Staged, crash-safe segment compaction.
//!
//! New segments are built in a hidden staging directory. Their fsyncs complete
//! before files move to the data directory; only a subsequent atomic manifest
//! switch makes the new generation visible. A crash before that switch leaves
//! the old generation authoritative and intact.

use crate::index::HashIndex;
use crate::segment::{
    create_segment, read_block, segment_path, sync_directory, FRAME_HEADER_BYTES,
    SEGMENT_HEADER_BYTES,
};
use crate::{record_keys, Infohash, Location, Store, StoreError};
use dhtrove_storage::{encode_best, TorrentRecord};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;

pub(crate) struct Staged {
    pub(crate) dir: PathBuf,
    pub(crate) segments: BTreeMap<u64, u64>,
    pub(crate) index: HashIndex,
    pub(crate) records: u64,
    pub(crate) bytes: u64,
    writer: File,
    active: u64,
    max_segment_bytes: u64,
    peak_base: u64,
    quota: Option<u64>,
}

impl Staged {
    pub(crate) fn build(store: &Store) -> Result<Self, StoreError> {
        let dir = store.root.join(".dhtrove-compacting");
        fs::create_dir(&dir)?;
        let first = store
            .segments
            .last_key_value()
            .ok_or(StoreError::Invalid("no active segments"))?
            .0
            .checked_add(1)
            .ok_or(StoreError::Invalid("segment ID overflow"))?;
        let writer = create_segment(&dir, first)?;
        let mut staged = Self {
            dir,
            segments: BTreeMap::from([(first, SEGMENT_HEADER_BYTES)]),
            index: HashIndex::new(),
            records: 0,
            bytes: SEGMENT_HEADER_BYTES,
            writer,
            active: first,
            max_segment_bytes: store.config.max_segment_bytes,
            peak_base: store.disk_bytes,
            quota: store.config.max_disk_bytes,
        };
        staged.check_peak(0)?;

        // Sorting locations groups repeated v1/v2 keys and metadata blocks,
        // so each source frame is decompressed at most once.
        let mut places: Vec<Location> = store.index.values().collect();
        places.sort_unstable_by_key(|p| (p.segment, p.offset, p.ordinal));
        places.dedup();

        let mut cached_key = None;
        let mut cached_records = Vec::new();
        let mut batch = Vec::new();
        let mut estimate = 0_usize;
        let target = store
            .config
            .max_segment_bytes
            .saturating_sub(SEGMENT_HEADER_BYTES + FRAME_HEADER_BYTES + 64)
            .min(1_048_576) as usize;

        for place in places {
            let frame_key = (place.segment, place.offset);
            if cached_key != Some(frame_key) {
                let mut input = File::open(segment_path(&store.root, place.segment))?;
                cached_records = read_block(&mut input, place.offset)?;
                cached_key = Some(frame_key);
            }
            let record = cached_records.get(place.ordinal).ok_or_else(|| {
                StoreError::Corrupt("compaction index ordinal out of bounds".into())
            })?;
            for key in record_keys(record) {
                if store.index.get(&key) != Some(place) {
                    return Err(StoreError::Corrupt(
                        "compaction index and source record disagree".into(),
                    ));
                }
            }
            let cost = estimated_size(record);
            if !batch.is_empty() && (batch.len() >= 64 || estimate.saturating_add(cost) > target) {
                staged.append_batch(&batch)?;
                batch.clear();
                estimate = 0;
            }
            estimate = estimate.saturating_add(cost);
            batch.push(record.clone());
        }
        if !batch.is_empty() {
            staged.append_batch(&batch)?;
        }
        staged.writer.sync_all()?;
        sync_directory(&staged.dir)?;
        Ok(staged)
    }

    fn check_peak(&self, additional: u64) -> Result<(), StoreError> {
        if let Some(limit) = self.quota {
            let used = self.peak_base.saturating_add(self.bytes);
            if used > limit || additional > limit - used {
                return Err(StoreError::Quota {
                    used,
                    needed: additional,
                    limit,
                });
            }
        }
        Ok(())
    }

    fn append_batch(&mut self, records: &[TorrentRecord]) -> Result<(), StoreError> {
        let (_, block) = encode_best(records)?;
        let frame_len = FRAME_HEADER_BYTES + block.len() as u64;
        if frame_len + SEGMENT_HEADER_BYTES > self.max_segment_bytes {
            if records.len() == 1 {
                return Err(StoreError::Invalid(
                    "record cannot fit in a compacted segment",
                ));
            }
            let middle = records.len() / 2;
            self.append_batch(&records[..middle])?;
            return self.append_batch(&records[middle..]);
        }
        let current = *self
            .segments
            .get(&self.active)
            .ok_or(StoreError::Invalid("missing compaction segment"))?;
        let rollover = current + frame_len > self.max_segment_bytes;
        let growth = frame_len + if rollover { SEGMENT_HEADER_BYTES } else { 0 };
        self.check_peak(growth)?;

        if rollover {
            self.writer.sync_all()?;
            let next = self
                .active
                .checked_add(1)
                .ok_or(StoreError::Invalid("compaction segment ID overflow"))?;
            self.writer = create_segment(&self.dir, next)?;
            self.active = next;
            self.segments.insert(next, SEGMENT_HEADER_BYTES);
            self.bytes += SEGMENT_HEADER_BYTES;
        }
        let offset = *self
            .segments
            .get(&self.active)
            .ok_or(StoreError::Invalid("missing compaction segment"))?;
        let length = u32::try_from(block.len())
            .map_err(|_| StoreError::Invalid("compaction block too large"))?;
        self.writer.seek(SeekFrom::Start(offset))?;
        self.writer.write_all(&length.to_le_bytes())?;
        self.writer
            .write_all(&crc32fast::hash(&block).to_le_bytes())?;
        self.writer.write_all(&block)?;
        self.bytes += frame_len;
        *self
            .segments
            .get_mut(&self.active)
            .ok_or(StoreError::Invalid("missing compaction segment"))? += frame_len;

        for (ordinal, record) in records.iter().enumerate() {
            let location = Location {
                segment: self.active,
                offset,
                ordinal,
            };
            for key in record_keys(record) {
                if self.index.insert(key, location).is_some() {
                    return Err(StoreError::Corrupt(
                        "duplicate record while compacting".into(),
                    ));
                }
            }
            self.records += 1;
        }
        Ok(())
    }

    /// Move fsynced segments from the staging directory to the active data
    /// directory. This does NOT switch the authoritative manifest.
    pub(crate) fn move_into(&self, root: &std::path::Path) -> Result<(), StoreError> {
        for &id in self.segments.keys() {
            fs::rename(segment_path(&self.dir, id), segment_path(root, id))?;
        }
        sync_directory(root)?;
        Ok(())
    }
}

fn estimated_size(record: &TorrentRecord) -> usize {
    let base = record.name.len().saturating_add(64);
    record.files.iter().fold(base, |acc, file| {
        acc.saturating_add(file.path.len().saturating_add(12))
    })
}
