# Append-only persistent segments (experimental)

DHTrove's first persistent storage prototype lives in \`dhtrove-store\`. It is
not yet a database service. It stores *metadata*, not torrent payload files.

## Basic example

\`\`\`rust
use dhtrove_storage::{TorrentRecord, FileEntry};
use dhtrove_store::{Infohash, Store, StoreConfig};

let mut store = Store::open("./dhtrove-data", StoreConfig::default())?;
let torrent = TorrentRecord {
    v1: Some([1; 20]),
    v2: None,
    name: b"Sample".to_vec(),
    files: vec![FileEntry { path: b"Sample/file.mkv".to_vec(), length: 42 }],
};
store.append(&[torrent.clone()])?;
assert_eq!(store.get(&Infohash::V1([1; 20]))?, Some(torrent));
# Ok::<(), Box<dyn std::error::Error>>(())
\`\`\`

## Layout

\`\`\`text
dhtrove-data/
  .dhtrove.lock
  segment-0000000000000000.dhs
  segment-0000000000000001.dhs
  ...
\`\`\`

Segment header: \`DHTS\`, version byte \`1\`, three zero bytes.
Each frame: 4-byte little-endian payload length, 4-byte CRC32, followed by a
full \`DHTB\` codec block. Multiple torrents share a frame, so prefix and token
compression can operate across torrent boundaries.

Every segment is immutable after rollover; only the last segment may be
extended. The active writer holds an exclusive advisory file lock.

## Crash recovery

The writer appends the frame and calls \`sync_data\` before exposing its new
hashes in the in-memory lookup index. On open, frames are scanned and validated.
If the final frame in the **last segment** is incomplete, it is truncated to the
last verified position. Full frames with bad checksums or invalid codec data
produce an error; they are **not silently deleted**. Partial frames in older
segments also fail rather than being silently discarded.

A sync failure poisons the writer; close/reopen to recover. On Unix, new
segment directory entries are fsynced. Other platforms may offer weaker
directory durability guarantees.

## Disk accounting and quotas

\`max_segment_bytes\` controls segment rotation. \`max_disk_bytes\` enforces an
exact upper bound on the *logical lengths of segment files* at append time.
It counts segment headers and frames, but **not** filesystem allocation
rounding, the lock file, OS metadata, future indexes, or temporary files.
It should **not** be interpreted as a physical filesystem capacity guarantee.

The current index resides in RAM and is rebuilt by scanning segments on every
open. It maps both hash versions to one stored record (no duplicate blob).
Existing hashes are not overwritten; inserting the same torrent is a no-op.

## Deliberate limitations

- No persistent hash index or optimized cold-start yet.
- No deletions, tombstones, compaction, or reclaiming space yet.
- No multi-process readers or distributed writers.
- No cross-block dictionaries.
- In-memory index uses \`HashMap\`; its RAM usage is not yet optimized.
- The index is not a full-text search index.
- Crash consistency relies on operating system / storage durability behavior.
- Disk quotas don't cover future secondary indexes or transient files.

The next milestone is a persistent, compact index with startup acceleration
and crash-safe checkpointing, followed by tombstones and compaction.
