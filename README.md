# DHTrove

**A storage-first, self-hosted BitTorrent DHT indexer written in Rust.**

DHTrove is an independent project inspired by the functionality of bitmagnet. Its primary objective is to index and search large BitTorrent metadata collections while minimizing total disk usage, including metadata, file trees, indexes, and operational state.

> Early development: storage format and benchmarks first. No DHT crawler or web API is implemented yet.

## Design principles

- **Space efficiency first:** measure *total* on-disk cost, never just compressed payload size.
- **Lossless metadata:** preserve exact path bytes, file order, file sizes, and v1/v2 infohashes.
- **Adaptive encoding:** compare raw storage, prefix compression, token dictionaries, and Zstd; avoid compression that expands data.
- **Reproducible evidence:** GitHub Actions runs tests, lint checks, and representative benchmarks.
- **Independent architecture:** clean Rust workspace with modular storage, crawler, search, and API components to follow.

## Roadmap

1. Storage block format, reversible codecs, round-trip tests, benchmarks.
2. Persistent block store, crash recovery, compaction and indexed lookup.
3. DHT discovery, metadata fetch, deduplication and scheduling.
4. Search and APIs (including Torznab), web UI, Docker distribution.

Performance claims will only be published after comparing against real, diverse, reproducible datasets.

## License

Apache-2.0.
