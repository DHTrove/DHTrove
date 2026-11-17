# DHT crawler core (experimental)

DHTrove's crawler is a **library-based** Rust service: no Docker entrypoint,
CLI, Telegram bot or web UI is hardwired to it. Those integration shells will
be chosen after the engine and real-world workload are reviewed.

## Components

- dhtrove-dht: UDP IPv4 node for BEP 5/KRPC, routing via XOR distance,
  opportunistic iterative traversal, BEP 51 sample_infohashes, inbound
  get_peers and authenticated announce_peer, bounded iterative get_peers.
- dhtrove-metadata: TCP BitTorrent/BEP 10 extension handshake, BEP 9
  ut_metadata pipelined 16 KiB piece acquisition, hash-verified v1/v2/hybrid
  info decoding. Torrent *payload* pieces are never requested.
- dhtrove-crawler: deduplicated work scheduling, per-job retry/cooldown,
  bounded metadata fetches, independent blocking writer to dhtrove-store,
  cross-torrent compression batches, checkpoint at configured intervals.

## Embedding

A future executable, bot or UI can embed the same engine:

    use dhtrove_crawler::{Crawler, CrawlerConfig};
    let config = CrawlerConfig::default();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let service = Crawler::new(config)?;
    let report = service.run(stop_rx).await?;
    // Another task can request shutdown via stop_tx.send(true).

This is a usage sketch, not a complete executable. The service is meant to
run inside an existing Tokio runtime. Drop the sender or send true to stop.

## Discovery

1. Resolve configurable bootstrap nodes.
2. Probe contacts through sample_infohashes (BEP 51). Parse samples and
   neighbors from replies; fall back to find_node for unsupported nodes.
3. Track incoming get_peers requests and verified announce_peer calls.
4. Schedule a bounded get_peers traversal for a discovered hash. Responses
   can contain compact peer addresses or additional nodes to query.
5. Connect to peers over TCP, request only ut_metadata, verify the complete
   info dictionary against the observed SHA-1 or SHA-256 (truncated to
   20 bytes for v2 DHT lookup), and persist the extracted filenames/sizes.
6. Reuse existing store indexes to avoid repeated fetches for known v1 hashes,
   and rely on the store's v1/v2 deduplication for hybrid records.

The DHT is distributed and does not offer an exhaustive listing. BEP 51
samples, inbound traffic, NAT/firewalls and network availability determine
the actual coverage. This implementation currently has an **IPv4 transport**
and **TCP metadata fetch** (not uTP or IPv6). DHT peer lists are volatile;
a single sample does not guarantee that an available metadata peer exists.

## Network hygiene

- Maximum 2 KiB incoming UDP packet, limited outgoing response size.
- Per-source IPv4 query rate limit and bounded pending RPC table.
- Timeouts for UDP RPCs, TCP connections and complete metadata exchange.
- Maximum 4 MiB torrent metadata by default, maximum BEP 9 piece/frame size.
- Address validation rejects common private and special-purpose addresses
  learned from untrusted nodes (opt-in private address mode for development).
- Token validation binds announce_peer to the requester IP, with rotating
  secrets and acceptance of the previous secret.
- All queues, jobs, active fetches, and peer lists are bounded.
- Only responding nodes enter the routing cache; unverified neighbor
  addresses are probed before promotion.
- Remote sample intervals and request retries are rate-limited.

## Remaining issues before a production deployment

This is **the first end-to-end implementation**, not yet a proven
production-grade crawler. In particular:

- Live public-DHT interoperability and saturation tests have NOT been run.
- The current IPv4 routing cache is bounded and per-XOR-prefix, not a fully
  persisted Kademlia k-bucket implementation with BEP 42 secure node IDs.
- No persistent DHT work queue/candidate cache across process restarts.
- No IPv6 DHT, uTP metadata transport or incoming BitTorrent TCP listener.
- v2-only torrents require a compatible peer responding via BEP 9; BEP 52
  piece layers are outside the info dictionary and are not validated here.
- Network throughput and disk/RAM usage under millions of real hashes remain
  unmeasured. The max queue and cache limits are deliberate safety bounds.
- Mixed v2/v1 DHT hash collisions are not represented by a separate
  20-byte kind in the work queue (practically negligible at SHA-length).
- A future stable service should offer structured metrics, restart
  persistence, observability, deployment controls and moderation policies.

## Testing strategy

CI includes parser and format unit tests. An actual public-DHT campaign
should happen only after packaging and deployment parameters are selected,
with controlled query rates, measured metadata success rates, and safeguards
against disk exhaustion. Personal servers are **not** used for builds/tests;
GitHub Actions is the designated validation environment.
