# Crash-safe segment compaction (experimental)

DHTrove supports persistent logical deletion via \`DHTD\` tombstone frames.
A tombstone makes the torrent inaccessible immediately, but physical space
is reclaimed when \`Store::compact()\` rewrites live records.

## Generation switch

The active segment set is recorded in a CRC-protected \`segments.manifest\`
file. It lists strictly ascending segment IDs. Legacy databases without a
manifest initially treat every \`segment-*.dhs\` file as active; DHTrove
publishes an explicit old-generation manifest before moving any compacted
segments into the data directory.

Compaction:

1. Acquires the existing exclusive writer lock (the store already owns it).
2. Enumerates unique active block locations from the infohash index.
3. Reads source frames once per frame, retaining only live records.
4. Re-encodes batches of records using adaptive block codecs into
   \`.dhtrove-compacting/\` with new, noncolliding segment IDs.
5. Fsyncs the new segment files and their directory.
6. Moves the new files into the data directory, then fsyncs it.
7. Atomically replaces \`segments.manifest\` to publish the new generation.
8. Replaces the in-memory index and writer, discards the stale checkpoint,
   and deletes old-generation segment files.

The manifest is the authoritative commit point. A crash before publication
leaves the old generation valid; a crash after publication makes the new
generation valid. On reopening, abandoned stage files and orphaned segments
are removed while holding the exclusive lock.

## Cost and quota

Compaction can require temporary disk space comparable to the size of the
resulting live dataset. When \`max_disk_bytes\` is set, compaction refuses
to exceed the logical quota at peak as it creates new segment files, and may
require the administrator to temporarily raise the limit.

\`CompactionOutcome\` reports whether a rewrite was performed and the old,
new and reclaimed logical bytes. If the replacement would not reduce segment
bytes, compaction skips the swap.

Metadata is not dropped from surviving torrents. Multiple hybrid infohash
keys still point to the same source record. All record names and file paths
are stored losslessly.

## Limitations

- Segment copying presently sorts in-memory locations to deduplicate hybrid
  references and uses bounded batches for encoding; this is not yet
  memory-optimal at 10M+ torrents.
- The active index is still held in RAM.
- Directory / manifest / checkpoint temporary file overhead and filesystem
  allocation rounding aren't included in \`max_disk_bytes\`.
- Some non-Unix platforms provide weaker directory sync guarantees.
- The format has no backward-compatibility guarantee before 1.0.
