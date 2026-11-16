# DHTrove

**Storage-first BitTorrent DHT indexing and search, built in Rust.**

DHTrove is a new, independent open-source project inspired by the *features*
of bitmagnet. Its primary objective is minimizing the **total disk footprint**
of a large BitTorrent metadata index, without sacrificing lossless metadata or
searchability. This is **not** a fork of bitmagnet.

> **Status: early prototype.** DHT discovery, persistence, API, search, and web
> interface are not implemented yet. Current code establishes the storage
> format experiments, correctness tests, and reproducible codec benchmarks.

## Implemented in this first iteration

- Independent Rust workspace with a storage library and benchmark CLI.
- Multiple torrent records per block, with v1 / v2 / hybrid infohashes.
- Lossless raw byte names and paths (UTF-8 is **not** required).
- Ordered file lists and 64-bit file lengths.
- Six encodings: raw, prefix, dictionary and each with Zstd.
- **Adaptive selection** by actual final encoded block size, including the
  dictionary, headers and CRC32.
- Local dictionary shared by *all torrents within each block*.
- Versioned envelope, checksum, upper bounds, corrupted-input tests.
- GitHub Actions CI for Rust formatting, Clippy, tests and benchmarks.

## Workspace

- `crates/dhtrove-storage` — reversible block encoders and decoders.
- `crates/dhtrove-bench` — deterministic, synthetic corpus benchmark.
- `docs/storage-format.md` — current format, trade-offs and limitations.

## Run on a development machine

```sh
cargo test --workspace --all-targets
cargo run --release -p dhtrove-bench -- 2000
```

CI automatically runs the checks and uploads a Markdown benchmark artifact.
Personal/self-hosted servers are **not** used for CI or builds.

## Guiding principles

1. **Space first:** account for metadata, file lists, search indexes and
   operational overhead, not merely the compressed strings.
2. **No silent data loss:** always round-trip original path bytes and file order.
3. **Benchmark against baselines:** a custom dictionary is useful only if it
   beats raw, prefix and established compression in measured workloads.
4. **Independent design:** storage, crawler, search, API and UI as modular crates.
5. **Avoid premature size claims:** the 20–30 GB per 10M torrents ambition
   remains a hypothesis until validated on a representative real dataset.

## Roadmap

1. Crash-safe persistent block store, compaction, indexing and quotas.
2. Real-corpus benchmarks and adaptive block sizing/global dictionary studies.
3. DHT discovery, peer metadata retrieval and deduplication.
4. Search, Torznab/API, web interface, Docker and production tooling.

Read [the storage format design](docs/storage-format.md) for details.

## License

Apache-2.0. See [LICENSE](LICENSE).
