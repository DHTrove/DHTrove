//! Storage-backed DHT crawler core. No binary, Docker, bot or UI is required.
//!
//! Embedders control lifecycle through a Tokio watch channel. All persistence
//! happens in a single blocking writer thread; the Tokio UDP/TCP runtime is
//! never blocked by compression, fsync, or checkpoint construction.

use dhtrove_dht::{DhtConfig, DhtNode, Id, Sighting};
use dhtrove_metadata::peer::{self, FetchConfig};
use dhtrove_storage::TorrentRecord;
use dhtrove_store::{Infohash, Store, StoreConfig, StoreError};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::interval;

#[derive(Clone, Debug)]
pub struct CrawlerConfig {
    pub store_path: PathBuf,
    pub store: StoreConfig,
    pub dht: DhtConfig,
    pub fetch: FetchConfig,
    pub sighting_buffer: usize,
    pub max_active_fetches: usize,
    pub max_queued_hashes: usize,
    pub max_peers_per_torrent: usize,
    pub max_fetch_attempts: u8,
    pub failure_retry_delay: Duration,
    pub checkpoint_every: usize,
}

impl Default for CrawlerConfig {
    fn default() -> Self {
        Self {
            store_path: PathBuf::from("./dhtrove-data"),
            store: StoreConfig::default(),
            dht: DhtConfig::default(),
            fetch: FetchConfig::default(),
            sighting_buffer: 16_384,
            max_active_fetches: 32,
            max_queued_hashes: 32_768,
            max_peers_per_torrent: 12,
            max_fetch_attempts: 3,
            failure_retry_delay: Duration::from_secs(60),
            checkpoint_every: 2_048,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CrawlReport {
    pub sightings: u64,
    pub metadata_fetched: u64,
    pub fetch_failures: u64,
    pub records_inserted: u64,
    pub duplicates_skipped: u64,
    pub bytes_written: u64,
    pub dht_packets: u64,
    pub dht_queries: u64,
    pub verified_nodes: usize,
    pub active_fetches: usize,
    pub queued_hashes: usize,
}

/// Thread-safe, live metrics shared with API/frontends without holding the
/// crawler's network loop or disk writer lock.
#[derive(Default)]
pub struct CrawlerMetrics {
    sightings: AtomicU64,
    fetched: AtomicU64,
    failures: AtomicU64,
    inserted: AtomicU64,
    duplicates: AtomicU64,
    written: AtomicU64,
    dht_packets: AtomicU64,
    dht_queries: AtomicU64,
    verified_nodes: AtomicU64,
    active_fetches: AtomicU64,
    queued_hashes: AtomicU64,
}
impl CrawlerMetrics {
    pub fn snapshot(&self) -> CrawlReport {
        let read = |n: &AtomicU64| n.load(Ordering::Relaxed);
        CrawlReport {
            sightings: read(&self.sightings),
            metadata_fetched: read(&self.fetched),
            fetch_failures: read(&self.failures),
            records_inserted: read(&self.inserted),
            duplicates_skipped: read(&self.duplicates),
            bytes_written: read(&self.written),
            dht_packets: read(&self.dht_packets),
            dht_queries: read(&self.dht_queries),
            verified_nodes: read(&self.verified_nodes) as usize,
            active_fetches: read(&self.active_fetches) as usize,
            queued_hashes: read(&self.queued_hashes) as usize,
        }
    }
}

#[derive(Debug)]
pub enum CrawlerError {
    InvalidConfig(&'static str),
    Io(std::io::Error),
    Store(StoreError),
    Worker(String),
}
impl fmt::Display for CrawlerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(text) => write!(f, "crawler config: {text}"),
            Self::Io(err) => write!(f, "crawler network: {err}"),
            Self::Store(err) => write!(f, "crawler storage: {err}"),
            Self::Worker(err) => write!(f, "crawler task: {err}"),
        }
    }
}
impl std::error::Error for CrawlerError {}
impl From<std::io::Error> for CrawlerError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<StoreError> for CrawlerError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

struct Job {
    peers: Vec<std::net::SocketAddrV4>,
    hints: Vec<std::net::SocketAddrV4>,
    attempts: u8,
    not_before: Instant,
}

impl Job {
    fn new() -> Self {
        Self {
            peers: Vec::new(),
            hints: Vec::new(),
            attempts: 0,
            not_before: Instant::now(),
        }
    }
    fn add(&mut self, sighting: Sighting, max: usize) {
        if let Some(peer) = sighting.peer {
            if !self.peers.contains(&peer) && self.peers.len() < max {
                self.peers.push(peer);
            }
        }
        if let Some(hint) = sighting.hint {
            if !self.hints.contains(&hint) && self.hints.len() < max {
                self.hints.push(hint);
            }
        }
    }
}

enum Command {
    Exists(Id, oneshot::Sender<bool>),
    Insert(TorrentRecord),
    Close,
}

#[derive(Default)]
struct WriterTotals {
    inserted: u64,
    duplicates: u64,
    written: u64,
}

fn writer(
    config: CrawlerConfig,
    mut rx: mpsc::Receiver<Command>,
    ready: oneshot::Sender<Result<(), String>>,
    metrics: Arc<CrawlerMetrics>,
) -> Result<WriterTotals, StoreError> {
    let mut store = match Store::open(&config.store_path, config.store) {
        Ok(store) => store,
        Err(err) => {
            let _ = ready.send(Err(err.to_string()));
            return Err(err);
        }
    };
    let _ = ready.send(Ok(()));
    let mut totals = WriterTotals::default();
    let mut deferred = None;
    let mut since_checkpoint = 0usize;
    loop {
        let Some(command) = deferred.take().or_else(|| rx.blocking_recv()) else {
            break;
        };
        match command {
            Command::Exists(hash, sender) => {
                let _ = sender.send(store.contains_hash(&Infohash::V1(hash)));
            }
            Command::Insert(first) => {
                // A batch is compressed *across torrents*, taking advantage
                // of shared filenames and dictionaries in existing codecs.
                let mut batch = vec![first];
                while batch.len() < 64 {
                    match rx.try_recv() {
                        Ok(Command::Insert(item)) => batch.push(item),
                        Ok(other) => {
                            deferred = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                let result = store.append(&batch)?;
                totals.inserted += result.inserted as u64;
                totals.duplicates += result.skipped_duplicates as u64;
                totals.written += result.bytes_written;
                metrics
                    .inserted
                    .fetch_add(result.inserted as u64, Ordering::Relaxed);
                metrics
                    .duplicates
                    .fetch_add(result.skipped_duplicates as u64, Ordering::Relaxed);
                metrics
                    .written
                    .fetch_add(result.bytes_written, Ordering::Relaxed);
                since_checkpoint += result.inserted;
                if config.checkpoint_every > 0 && since_checkpoint >= config.checkpoint_every {
                    store.checkpoint()?;
                    since_checkpoint = 0;
                }
            }
            Command::Close => break,
        }
    }
    // Checkpoint if writes occurred; redundant empty stores are not checkpointed.
    if since_checkpoint > 0 {
        store.checkpoint()?;
    }
    Ok(totals)
}

enum FetchResult {
    Existing(Id),
    Fetched(Id),
    Failed(Id),
}

struct FetchTask {
    node: DhtNode,
    hash: Id,
    peers: Vec<std::net::SocketAddrV4>,
    hints: Vec<std::net::SocketAddrV4>,
    config: FetchConfig,
    max_peers: usize,
    peer_id: [u8; 20],
    storage: mpsc::Sender<Command>,
}

/// Try a few metadata peers in parallel. Stop on the first verified infohash,
/// and cancel losing connections rather than waiting for serial timeouts.
async fn try_peers(
    addresses: Vec<std::net::SocketAddrV4>,
    hash: Id,
    peer_id: [u8; 20],
    config: FetchConfig,
) -> Option<TorrentRecord> {
    let mut queue: VecDeque<_> = addresses.into_iter().collect();
    let mut in_flight = JoinSet::new();
    while !queue.is_empty() || !in_flight.is_empty() {
        while in_flight.len() < 3 {
            let Some(peer) = queue.pop_front() else { break };
            in_flight.spawn(async move { peer::fetch(peer, hash, peer_id, config).await });
        }
        let Some(result) = in_flight.join_next().await else {
            break;
        };
        if let Ok(Ok(record)) = result {
            in_flight.abort_all();
            return Some(record);
        }
    }
    None
}

async fn fetch_one(task: FetchTask) -> FetchResult {
    let FetchTask {
        node,
        hash,
        peers,
        hints,
        config,
        max_peers,
        peer_id,
        storage,
    } = task;

    let (tx, rx) = oneshot::channel();
    if storage.send(Command::Exists(hash, tx)).await.is_err() {
        return FetchResult::Failed(hash);
    }
    if rx.await.ok() == Some(true) {
        return FetchResult::Existing(hash);
    }

    let mut tried = HashSet::new();
    let direct: Vec<_> = peers
        .into_iter()
        .filter(|peer| tried.insert(*peer))
        .take(max_peers)
        .collect();
    // Announce-derived peers should be attempted immediately; there is no
    // reason to wait for a full get_peers lookup when a candidate is ready.
    if let Some(record) = try_peers(direct, hash, peer_id, config).await {
        return if storage.send(Command::Insert(record)).await.is_ok() {
            FetchResult::Fetched(hash)
        } else {
            FetchResult::Failed(hash)
        };
    }

    let discovered: Vec<_> = node
        .find_peers(hash, &hints)
        .await
        .into_iter()
        .filter(|peer| tried.insert(*peer))
        .take(max_peers)
        .collect();
    if let Some(record) = try_peers(discovered, hash, peer_id, config).await {
        return if storage.send(Command::Insert(record)).await.is_ok() {
            FetchResult::Fetched(hash)
        } else {
            FetchResult::Failed(hash)
        };
    }
    FetchResult::Failed(hash)
}

pub struct Crawler {
    config: CrawlerConfig,
}

impl Crawler {
    pub fn new(config: CrawlerConfig) -> Result<Self, CrawlerError> {
        if config.max_active_fetches == 0
            || config.max_queued_hashes == 0
            || config.sighting_buffer == 0
            || config.max_peers_per_torrent == 0
            || config.max_fetch_attempts == 0
        {
            return Err(CrawlerError::InvalidConfig(
                "all queue and worker limits must be positive",
            ));
        }
        Ok(Self { config })
    }

    /// Run until the shutdown sender changes to true or drops.
    /// Network integration tests are separate from codec unit tests.
    pub async fn run(self, shutdown: watch::Receiver<bool>) -> Result<CrawlReport, CrawlerError> {
        self.run_with_metrics(shutdown, Arc::new(CrawlerMetrics::default()))
            .await
    }

    pub async fn run_with_metrics(
        self,
        mut shutdown: watch::Receiver<bool>,
        metrics: Arc<CrawlerMetrics>,
    ) -> Result<CrawlReport, CrawlerError> {
        let (storage_tx, storage_rx) = mpsc::channel(4_096);
        let (ready_tx, ready_rx) = oneshot::channel();
        let writer_cfg = self.config.clone();
        let writer_metrics = metrics.clone();
        let writer_handle = tokio::task::spawn_blocking(move || {
            writer(writer_cfg, storage_rx, ready_tx, writer_metrics)
        });
        match ready_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => return Err(CrawlerError::Worker(reason)),
            Err(_) => return Err(CrawlerError::Worker("storage worker startup failed".into())),
        }
        let (sighting_tx, mut sightings) = mpsc::channel(self.config.sighting_buffer);
        let node = DhtNode::bind(self.config.dht.clone(), sighting_tx).await?;
        let node_clone = node.clone();
        let node_shutdown = shutdown.clone();
        let mut node_handle = tokio::spawn(async move { node_clone.run(node_shutdown).await });
        let peer_id: [u8; 20] = rand::random();

        let mut jobs: HashMap<Id, Job> = HashMap::new();
        let mut pending = VecDeque::new();
        let mut active = HashSet::new();
        let mut recent = HashMap::new();
        let mut fetches = JoinSet::new();
        let mut pulse = interval(Duration::from_millis(50));
        let mut report = CrawlReport::default();

        loop {
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
                Some(observation) = sightings.recv() => {
                    report.sightings += 1;
                    metrics.sightings.fetch_add(1, Ordering::Relaxed);
                    let hash = observation.infohash;
                    if recent.get(&hash).is_some_and(|seen: &Instant| seen.elapsed() < Duration::from_secs(12*3600)) {
                        continue;
                    }
                    if let Some(job) = jobs.get_mut(&hash) {
                        job.add(observation, self.config.max_peers_per_torrent);
                    } else if jobs.len() < self.config.max_queued_hashes {
                        let mut job = Job::new();
                        job.add(observation, self.config.max_peers_per_torrent);
                        jobs.insert(hash, job);
                        pending.push_back(hash);
                    }
                    if recent.len() > 100_000 {
                        recent.retain(|_, time: &mut Instant| time.elapsed() < Duration::from_secs(12*3600));
                    }
                }
                _ = pulse.tick() => {
                    let dht = node.stats();
                    metrics.dht_packets.store(dht.packets_received, Ordering::Relaxed);
                    metrics.dht_queries.store(dht.queries_sent, Ordering::Relaxed);
                    metrics.verified_nodes.store(dht.verified_nodes as u64, Ordering::Relaxed);
                    metrics.active_fetches.store(fetches.len() as u64, Ordering::Relaxed);
                    metrics.queued_hashes.store(pending.len() as u64, Ordering::Relaxed);
                    if fetches.len() >= self.config.max_active_fetches { continue; }
                    let limit = pending.len().min(32);
                    for _ in 0..limit {
                        if fetches.len() >= self.config.max_active_fetches { break; }
                        let Some(hash) = pending.pop_front() else { break; };
                        if active.contains(&hash) { continue; }
                        let Some(job) = jobs.get(&hash) else { continue; };
                        if Instant::now() < job.not_before {
                            pending.push_back(hash);
                            continue;
                        }
                        active.insert(hash);
                        let peers = job.peers.clone();
                        let hints = job.hints.clone();
                        let node = node.clone();
                        let store = storage_tx.clone();
                        let max = self.config.max_peers_per_torrent;
                        let fetch_cfg = self.config.fetch;
                        fetches.spawn(fetch_one(FetchTask {
                            node,
                            hash,
                            peers,
                            hints,
                            config: fetch_cfg,
                            max_peers: max,
                            peer_id,
                            storage: store,
                        }));
                    }
                }
                Some(result) = fetches.join_next(), if !fetches.is_empty() => {
                    match result {
                        Ok(FetchResult::Fetched(hash)) => {
                            active.remove(&hash);
                            jobs.remove(&hash);
                            recent.insert(hash, Instant::now());
                            report.metadata_fetched += 1;
                            metrics.fetched.fetch_add(1, Ordering::Relaxed);
                        }
                        Ok(FetchResult::Existing(hash)) => {
                            active.remove(&hash);
                            jobs.remove(&hash);
                            recent.insert(hash, Instant::now());
                        }
                        Ok(FetchResult::Failed(hash)) => {
                            active.remove(&hash);
                            report.fetch_failures += 1;
                            metrics.failures.fetch_add(1, Ordering::Relaxed);
                            if let Some(job) = jobs.get_mut(&hash) {
                                job.attempts += 1;
                                if job.attempts >= self.config.max_fetch_attempts {
                                    jobs.remove(&hash);
                                    recent.insert(hash, Instant::now());
                                } else {
                                    job.not_before = Instant::now()
                                        + self.config.failure_retry_delay * u32::from(job.attempts);
                                    pending.push_back(hash);
                                }
                            }
                        }
                        Err(_) => {
                            return Err(CrawlerError::Worker("metadata task panicked".into()));
                        }
                    }
                }
                result = &mut node_handle => {
                    return Err(CrawlerError::Worker(format!("DHT runtime unexpectedly exited: {result:?}")));
                }
            }
        }
        fetches.abort_all();
        let _ = node_handle.await;
        let _ = storage_tx.send(Command::Close).await;
        drop(storage_tx);
        let totals = writer_handle
            .await
            .map_err(|err| CrawlerError::Worker(err.to_string()))??;
        report.records_inserted = totals.inserted;
        report.duplicates_skipped = totals.duplicates;
        report.bytes_written = totals.written;
        let dht = node.stats();
        metrics
            .dht_packets
            .store(dht.packets_received, Ordering::Relaxed);
        metrics
            .dht_queries
            .store(dht.queries_sent, Ordering::Relaxed);
        metrics
            .verified_nodes
            .store(dht.verified_nodes as u64, Ordering::Relaxed);
        metrics.active_fetches.store(0, Ordering::Relaxed);
        metrics.queued_hashes.store(0, Ordering::Relaxed);
        report.dht_packets = dht.packets_received;
        report.dht_queries = dht.queries_sent;
        report.verified_nodes = dht.verified_nodes;
        report.records_inserted = metrics.inserted.load(Ordering::Relaxed);
        report.duplicates_skipped = metrics.duplicates.load(Ordering::Relaxed);
        report.bytes_written = metrics.written.load(Ordering::Relaxed);
        Ok(report)
    }
}
