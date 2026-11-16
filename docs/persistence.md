# Append-only persistent segments (experimental)

DHTrove's persistent storage prototype lives in `dhtrove-store`. It stores
*metadata*, not the torrent payload files, and is not a full database service.

## Basic example

```rust
use dhtrove_storage::{FileEntry, TorrentRecord};
use dhtrove_store::{Infohash, Store, StoreConfig};

let mut store = Store::open("./dhtrove-data", StoreConfig::default())?;
let torrent = TorrentRecord {
    v1: Some([1; 20]),
    v2: None,
    name: b"Sample".to_vec(),
    files: vec![FileEntry { path: b"Sample/file.mkv".to_vec(), length: 42 }],
};
store.append(&[torrent.clone()])?;
store.checkpoint()?; // optional, speeds up a future startup
assert_eq!(store.get(&Infohash::V1([1; 20]))?, Some(torrent));
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Layout

```text
dhtrove-data/
  .dhtrove.lock
  hash-index.zst
  segment-0000000000000000.dhs
  segment-0000000000000001.dhs
  ...
```

A segment begins with `DHTS`, format version 1 and three zero bytes.
Each frame contains a four-byte little-endian payload length, four-byte CRC32
and a self-contained `DHTB` codec block. Multiple torrents may share one frame.

The active writer holds an exclusive advisory file lock. A segment is
immutable after rollover; only the last segment may grow.

## Crash recovery

The writer calls `sync_data` before exposing newly inserted records.
At startup, the index is either rebuilt by scanning all segment frames or
restored from a compact checkpoint, followed by replaying the suffix added
since the checkpoint was created.

If a final frame in the active segment is incomplete, it is truncated to the
last verified position. Complete corrupt frames and partial frames in sealed
segments are errors, **not** silently deleted.

A write failure poisons the store handle; close and reopen it for recovery.
New segment and checkpoint directory entries are fsynced on Unix. Other
platforms may offer weaker durability guarantees.

See [index checkpoints](checkpoints.md) for their validation and recovery
semantics. Checkpoints do not revalidate the contents of older segment frames.

## Disk accounting and quotas

`max_segment_bytes` limits each segment's logical length and controls rotation.
`max_disk_bytes` counts segment lengths plus the compressed checkpoint file.
Neither measures OS directory metadata, physical filesystem block allocation,
lock files, or temporary data created while replacing a checkpoint. A checkpoint
can temporarily require space for both old and new versions.

The RAM index maps both v1/v2 infohash keys to a single record for a hybrid
torrent. Existing hashes are immutable (first writer wins), so adding a
known torrent does not create another metadata blob.

## Deliberate limitations

- RAM HashMap still holds keys; checkpoints accelerate loading, not memory use.
- No deletion, tombstones or storage compaction yet.
- No multi-process readers or distributed writers.
- No shared dictionaries across blocks.
- No full-text search index.
- No hard physical disk reservations.
- Current formats are experimental and may change without migration.
