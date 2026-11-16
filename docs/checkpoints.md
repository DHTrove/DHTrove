# Persistent hash-index checkpoints (experimental)

DHTrove can save a compact Zstd-compressed snapshot of its in-memory infohash
lookup index. A checkpoint stores only the mapping **infohash -> segment,
frame offset, record ordinal**. Actual torrent paths, titles and file sizes
remain exclusively in the already-compressed segment frames.

## Format

File: \`hash-index.zst\` (Zstd frame with content checksum).

Uncompressed structure:

- Magic \`DHTI\`, version \`1\`, three reserved bytes.
- Segment count, followed by sorted segment ID and logical file length pairs.
- Total torrent record count and number of infohash keys.
- Sorted infohash keys: type (v1 or v2), 20/32 bytes of binary hash,
  8-byte segment ID, 8-byte frame offset and 4-byte record ordinal.

Hybrid torrents have two index keys pointing to the same record. As a
first iteration, entries use fixed-width locations and raw binary hashes,
compressed together with Zstd. A more compact mmap-friendly or delta-coded
index may be explored after measurement.

## Restart and recovery

\`Store::checkpoint()\` writes \`hash-index.zst.tmp\`, fsyncs its contents,
then atomically renames it over the previous checkpoint and syncs the parent
directory on Unix. On startup, abandoned \`.tmp\` files are removed.

The checkpoint is accepted when its recorded segment manifest matches a
**prefix** of the current segment files. The last checkpointed segment may
have grown, and any entirely new segments are replayed from the start. This
avoids a full metadata decompression pass if only a small tail was added.

A missing, corrupt or stale checkpoint is ignored; the index is rebuilt by
scanning all segments. The stale checkpoint file is removed after a successful
scan, so it will no longer occupy space. Original segment data is never
deleted in this fallback.

A valid checkpoint proves only that the index file is structurally sound
and describes segment lengths. It does **not** validate the stored segment
contents again on startup. Reads still verify segment checksums, and future
versions should add block integrity manifests.

## Accounting

\`StoreStats.disk_bytes\` is the sum of logical segment sizes **plus** the
current checkpoint file size. \`max_disk_bytes\` applies to that sum. It does
not include filesystem allocation granularity, directory entries, the
\`.dhtrove.lock\` file or **temporary space used while replacing a checkpoint**.
Creating a new checkpoint may briefly require space for both old and new
index files. It is not a strict physical disk reservation mechanism.

Checkpoints are explicit (not automatically written on every append). A
checkpoint remains useful across further appends because the loader replays
only the new suffix. Rebuilding a full checkpoint is an intentional task for
scheduled maintenance rather than every disk write.

## Limitations

- Hashes are still loaded into an in-memory HashMap at startup.
- Searches are by exact infohash, not full-text.
- Checkpoints are not authenticated against malicious modification.
- No tombstones, compacted deletions or range queries.
- No cross-process read-only snapshot guarantee yet.
