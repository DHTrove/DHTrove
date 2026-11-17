# Packed infohash index (experimental)

The original DHTrove prototype used a Rust HashMap for every v1/v2 infohash,
even when older records were immutable. That incurs hash-table buckets,
load-factor slack and entry padding. The packed index separates:

- A **frozen sorted array** of 40-byte v1 entries: the 20-byte SHA-1 infohash,
  8-byte segment number, 8-byte frame offset, 4-byte record ordinal.
- A **frozen sorted array** of 52-byte v2 entries: the 32-byte SHA-256 infohash,
  plus the same 20-byte physical location.
- An **overlay** of recent writes/deletions in a HashMap, storing None for
  deleted old keys and the latest location for newly inserted keys.

The stable arrays need exactly 40 bytes of entry payload per v1 key and
52 bytes per v2 key. With 10 million v1 hashes, that is 400 MB of packed entry
payload *before* vector slack, mutable overlay, file-cache, metadata blocks,
other indexes, allocator overhead or operating-system memory. This is not a
measurement of total application RAM.

Exact infohash lookups check the overlay first, then binary-search the
corresponding frozen array. Frozen lookups are O(log N); overlay lookups are
expected O(1). A hybrid torrent uses both v1 and v2 keys, both referring to
the exact same record location.

## On-disk compatibility

The existing Zstd-compressed checkpoint format is unchanged. Its keys are
already sorted, so the loader can stream them directly into the packed arrays
without constructing an intermediate full HashMap. Entries that are out of
order or repeated make the checkpoint invalid; the existing recovery code
falls back to scanning the segment data.

On opening a database without a checkpoint, the segment scan builds a mutable
index, which is packed once after recovery. On opening with a checkpoint,
new segment frames are replayed as an overlay and packed after recovery.

Calling Store::checkpoint() repacks the overlay and writes a new index
checkpoint. Repacking uses temporary RAM to sort all keys; it should happen
under controlled maintenance intervals in very large deployments.

Compaction also builds a fresh packed index and does not duplicate hybrid
torrent metadata.

## Tradeoffs and current limitations

- Frozen lookups use binary search rather than a hash-table probe, so
  individual lookups can have higher latency; benchmark using real workloads.
- Recent changes consume additional mutable HashMap memory until a
  checkpoint, compaction or reopen. There is no background merge yet.
- Repacking briefly uses extra memory; a future implementation should use
  an external sort or incremental merge to bound peak memory.
- Hash index remains in RAM; mmap-compatible on-disk indexes are a later goal.
- StoreStats.packed_index_bytes counts the frozen Vec backing capacities only;
  it does not include the overlay HashMap, Vec metadata or other allocations.
- No real 10-million-torrent RAM or disk benchmark has been run yet.

The comparison must consider total resident set size, startup time,
lookup latency, insert rate and peak memory during repacking.
