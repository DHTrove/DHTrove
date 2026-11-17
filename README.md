# DHTrove

**Storage-first BitTorrent DHT indexing and search, built in Rust.**

DHTrove is an independent open-source project inspired by the *features*
of bitmagnet. Its primary objective is minimizing the **total disk footprint**
of a large BitTorrent metadata index without sacrificing lossless metadata or
searchability. This is **not** a fork of bitmagnet.

> **Status: early release.** DHTrove now includes a headless service, Docker
> packaging and an authenticated web dashboard. Public-DHT performance testing,
> full-text search and the Torznab API are still future work.

## Implemented

- Rust workspace with separated codec, persistence and benchmark crates.
- Multiple torrent records in one independently decompressible block.
- v1 / v2 / hybrid infohashes; arbitrary byte names and paths, original file order.
- Six encodings: raw, prefix, dictionary, and each with Zstd.
- Adaptive block codec selection by *total encoded size*.
- Crash-recoverable append-only segment files with checksums and segment rotation.
- Packed, sorted infohash lookup (40-byte v1 / 52-byte v2 entries) with a mutable
  overlay and Zstd checkpoints; hybrid hashes share the same torrent metadata.
- Exclusive writer lock, durable tombstone deletion, and crash-safe segment
  compaction with an atomic active-segment manifest.
- IPv4 DHT networking (BEP 5 and BEP 51 sampling) and metadata-only TCP peer
  exchange (BEP 9/10), integrated through a bounded headless crawler library.
- GitHub Actions runs formatting, Clippy, round-trip and crash-recovery tests,
  plus reproducible synthetic compression benchmarks.

## Workspace

- `crates/dhtrove-storage` — lossless block codecs.
- `crates/dhtrove-store` — append-only persistence and infohash lookup.
- `crates/dhtrove-bench` — deterministic compression benchmark.
- `crates/dhtrove-dht` — async UDP node and iterative DHT discovery.
- `crates/dhtrove-metadata` — metadata-only TCP peer exchange and parsing.
- `crates/dhtrove-crawler` — bounded scheduling and persistent ingestion.
- `crates/dhtrove-server` — headless service, HTTP API and web dashboard.
- `docs/storage-format.md` — block codec prototype.
- `docs/persistence.md` — disk format, API, recovery and limitations.
- `docs/checkpoints.md` — index checkpoint format, replay and quota behavior.
- `docs/deletions.md` — tombstone framing and replay.
- `docs/compaction.md` — atomic generation switching and space reclamation.
- `docs/packed-index.md` — compact in-memory infohash index and tradeoffs.
- `docs/dht-crawler.md` — DHT traversal, metadata fetch and embedding.
- `docs/deployment.md` — Docker, manual start/stop, credentials and releases.

## Docker

Use the included Compose file to run a local web dashboard:

    docker compose up -d
    docker exec dhtrove cat /data/admin.token

Open http://localhost:8080, enter the token, and press **Start crawler**
when indexing is needed. The crawler is **stopped by default**, including
after container restarts. The panel stays available after you stop it.

Full documentation: [Deployment and operation](docs/deployment.md).

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

1. Benchmark total index RAM, search latency and checkpoint load time on real data.
2. Validate compaction at scale and improve memory efficiency; physical disk quotas.
3. Real representative dataset benchmarks and tuning adaptive codecs/block sizes.
4. Public-network interoperability/throughput and multi-million record tests.
5. IPv6 DHT, persistent discovery queue, full-text search, Torznab/API and web enhancements.

See the [persistence design](docs/persistence.md) and
[codec format](docs/storage-format.md).

## License

Apache-2.0. See [LICENSE](LICENSE).
