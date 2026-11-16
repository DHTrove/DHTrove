# DHTrove

**Storage-first BitTorrent DHT indexing and search, built in Rust.**

DHTrove is an independent open-source project inspired by the *features*
of bitmagnet. Its primary objective is minimizing the **total disk footprint**
of a large BitTorrent metadata index without sacrificing lossless metadata or
searchability. This is **not** a fork of bitmagnet.

> **Status: early prototype.** The block codecs and a persistent append-only
> segment store are implemented. DHT discovery, search, API and web UI are not.

## Implemented

- Rust workspace with separated codec, persistence and benchmark crates.
- Multiple torrent records in one independently decompressible block.
- v1 / v2 / hybrid infohashes; arbitrary byte names and paths, original file order.
- Six encodings: raw, prefix, dictionary, and each with Zstd.
- Adaptive block codec selection by *total encoded size*.
- Crash-recoverable append-only segment files with checksums and segment rotation.
- Infohash lookup with optional compressed checkpoints, replaying only new segment
  data at startup; hybrid hashes reference the same record without duplication.
- Exclusive writer lock, idempotent duplicate inserts, durable deletion tombstones,
  logical data and checkpoint quota.
- GitHub Actions runs formatting, Clippy, round-trip and crash-recovery tests,
  plus reproducible synthetic compression benchmarks.

## Workspace

- `crates/dhtrove-storage` — lossless block codecs.
- `crates/dhtrove-store` — append-only persistence and infohash lookup.
- `crates/dhtrove-bench` — deterministic compression benchmark.
- `docs/storage-format.md` — block codec prototype.
- `docs/persistence.md` — disk format, API, recovery and limitations.
- `docs/checkpoints.md` — index checkpoint format, replay and quota behavior.
- `docs/deletions.md` — tombstone framing, replay and limitations.

## Run on a development machine

```sh
cargo test --workspace --all-targets
cargo run --release -p dhtrove-bench -- 2000
```

CI automatically runs these checks on GitHub Actions. Personal/self-hosted
servers are not used for builds or tests.

## Guiding principles

1. **Space first:** count metadata, file trees, secondary indexes and operational
   overhead, not just compressed strings.
2. **No silent data loss:** preserve raw path bytes and file order.
3. **Benchmarks before claims:** compare against established compression.
4. **Modular Rust architecture:** storage, crawler, search, API and UI.
5. **No unsupported disk-size claims:** 20–30 GB per 10M torrents is an ambitious
   target, not a proven result.

## Roadmap

1. Profile index checkpoint size, memory usage and cold-start latency at scale.
2. Safe compaction to reclaim tombstoned data; physical disk quotas.
3. Real representative dataset benchmarks and tuning adaptive codecs/block sizes.
4. DHT discovery, peer metadata retrieval and deduplication.
5. Search, Torznab/API, web interface, Docker and production tooling.

See the [persistence design](docs/persistence.md) and
[codec format](docs/storage-format.md).

## License

Apache-2.0. See [LICENSE](LICENSE).
