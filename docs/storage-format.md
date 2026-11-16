# Storage prototype v1

This document describes a **benchmarkable prototype**, not a stable on-disk format.

## Goals and scope

- Store multiple distinct torrent records in one independently decodable block.
- Preserve v1 (SHA-1, 20 bytes), v2 (SHA-256, 32 bytes), and hybrid hashes.
- Keep original name/path bytes, file order, and 64-bit lengths unchanged.
- Compare encodings on their entire blocks, including the dictionary overhead.
- Detect accidental corruption and reject overlarge decoded content.

No torrent payloads or full .torrent files are stored. The format does not
include piece hashes, announce lists, trackers or a persistent database yet.

## Envelope

| Field | Representation |
|---|---|
| Magic | 4 ASCII bytes: DHTB |
| Version | 1 byte: 1 |
| Codec | 1 byte |
| Uncompressed payload length | unsigned base-128 varint |
| Payload CRC32 | 4 bytes, little-endian |
| Payload | raw or Zstd-compressed bytes |

Codec variants: raw, front-prefix, token dictionary, and their Zstd equivalents.
Codec selection tries each and picks the smallest fully encoded result.

Payload:

1. Dictionary entry count plus length-delimited byte tokens (dictionary codecs only).
2. Torrent record count.
3. For every record: flags for v1/v2, fixed-size hash bytes, encoded name,
   file count, then encoded path and unsigned 64-bit length for every file.

Paths in the prefix codec share the leading bytes with the *previous* name/path
in the block. The token dictionary is built across every record name and path
in the block (not only inside one torrent). Tokens are repeated sequences of
ASCII letters/digits and non-ASCII bytes. Literal runs and tokens reconstruct
the original sequence exactly without UTF-8 normalization.

## Defensive limits

- 64 MiB maximum block data (input and uncompressed).
- 20,000 torrents and 100,000 file entries per block.
- 64 KiB maximum individual path/name.
- 4,096 maximum dictionary entries.
- Bound checks are performed before allocation or append where applicable.

These are initial limits, not promises about eventual database capacity.
The persistent store will split large collections across many blocks.

## Benchmarking

Run with GitHub Actions, or manually on a separate development machine:

    cargo test --workspace --all-targets
    cargo run --release -p dhtrove-bench -- 2000

The benchmark uses synthetic, deterministic titles with intentional repetition.
It is for comparing codec implementations, **not** estimating the size of a real
10-million-torrent database. Production comparisons must measure at least:

- Different title languages and malformed byte names.
- Torrents with many tiny files versus a few large files.
- Real distribution of unique terms and path prefix repetition.
- Compression of the metadata plus lookup indexes, free space, and WAL.
- Query latency, indexing speed, and peak memory at the same dataset.

## Next steps

- Persist blocks with crash-safe writes, offsets and compact lookup indexes.
- Introduce block-size tuning and shared immutable dictionaries spanning blocks.
- Make compaction, dictionary rebuilds and disk quotas measurable.
- Add real anonymized/redistributable benchmark corpora.
- Profile before promoting any codec as the default.

v1 is explicitly **not** intended for long-term persistence or compatibility.
