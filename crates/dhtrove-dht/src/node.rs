use crate::bencode::{self, Value};
use crate::krpc::{self, Id, Message, NodeContact};
use crate::routing::Routing;
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::{lookup_host, UdpSocket};
use tokio::sync::{mpsc, oneshot, watch, Semaphore};
use tokio::task::JoinSet;
use tokio::time::{interval, timeout};

const MAX_PACKET: usize = 2048;
const MAX_SEEN: usize = 40_000;
const MAX_SAMPLE: usize = 24;

#[derive(Clone, Debug)]
pub struct DhtConfig {
    pub bind: SocketAddrV4,
    pub bootstrap: Vec<String>,
    pub query_timeout: Duration,
    pub query_interval: Duration,
    pub max_inflight_queries: usize,
    pub max_peers_per_hash: usize,
    pub lookup_budget: usize,
    pub inbound_queries_per_ip_second: u32,
    pub allow_private_addresses: bool,
}

impl Default for DhtConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 6881),
            bootstrap: vec![
                "router.bittorrent.com:6881".into(),
                "router.utorrent.com:6881".into(),
                "dht.transmissionbt.com:6881".into(),
            ],
            query_timeout: Duration::from_secs(4),
            query_interval: Duration::from_millis(200),
            max_inflight_queries: 64,
            max_peers_per_hash: 16,
            lookup_budget: 32,
            inbound_queries_per_ip_second: 40,
            allow_private_addresses: false,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Sighting {
    pub infohash: Id,
    /// Known advertised peer, or None if we only observed the hash.
    pub peer: Option<SocketAddrV4>,
    /// DHT node that reported this infohash and may know its peers.
    pub hint: Option<SocketAddrV4>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DhtStats {
    pub packets_received: u64,
    pub queries_sent: u64,
    pub replies_received: u64,
    pub discovered_hashes: u64,
    pub sightings_dropped: u64,
    pub verified_nodes: usize,
}

#[derive(Default)]
struct Counters {
    received: AtomicU64,
    sent: AtomicU64,
    replies: AtomicU64,
    discovered: AtomicU64,
    dropped: AtomicU64,
}

#[derive(Clone)]
struct RateEntry {
    since: Instant,
    count: u32,
}

#[derive(Clone)]
struct PeerSet {
    updated: Instant,
    peers: Vec<SocketAddrV4>,
}

struct TokenSecrets {
    latest: [u8; 20],
    prior: [u8; 20],
    since: Instant,
}

impl TokenSecrets {
    fn new() -> Self {
        Self {
            latest: rand::random(),
            prior: rand::random(),
            since: Instant::now(),
        }
    }
    fn rotate(&mut self) {
        if self.since.elapsed() >= Duration::from_secs(300) {
            self.prior = self.latest;
            self.latest = rand::random();
            self.since = Instant::now();
        }
    }
    fn hash(secret: &[u8; 20], ip: Ipv4Addr) -> [u8; 20] {
        let mut hasher = Sha1::new();
        hasher.update(ip.octets());
        hasher.update(secret);
        hasher.finalize().into()
    }
    fn issue(&mut self, ip: Ipv4Addr) -> [u8; 20] {
        self.rotate();
        Self::hash(&self.latest, ip)
    }
    fn valid(&mut self, ip: Ipv4Addr, presented: &[u8]) -> bool {
        self.rotate();
        (presented == Self::hash(&self.latest, ip) || presented == Self::hash(&self.prior, ip))
            && presented.len() == 20
    }
}

struct Inner {
    socket: UdpSocket,
    config: DhtConfig,
    own_id: Id,
    routing: Mutex<Routing>,
    pending: Mutex<HashMap<(SocketAddrV4, u16), oneshot::Sender<Vec<u8>>>>,
    candidate_tx: mpsc::Sender<SocketAddrV4>,
    sightings: mpsc::Sender<Sighting>,
    peers: Mutex<HashMap<Id, PeerSet>>,
    secrets: Mutex<TokenSecrets>,
    rates: Mutex<HashMap<Ipv4Addr, RateEntry>>,
    permits: Semaphore,
    counters: Counters,
}

#[derive(Clone)]
pub struct DhtNode {
    inner: Arc<Inner>,
    candidates: Arc<tokio::sync::Mutex<mpsc::Receiver<SocketAddrV4>>>,
}

struct Response {
    nodes: Vec<NodeContact>,
    peers: Vec<SocketAddrV4>,
    interval: Duration,
}

impl DhtNode {
    pub async fn bind(config: DhtConfig, sightings: mpsc::Sender<Sighting>) -> io::Result<Self> {
        if config.max_inflight_queries == 0
            || config.max_peers_per_hash == 0
            || config.lookup_budget == 0
            || config.query_interval.is_zero()
            || config.query_timeout.is_zero()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "DHT limits must be positive",
            ));
        }
        let socket = UdpSocket::bind(config.bind).await?;
        let id: Id = rand::random();
        let (candidate_tx, candidate_rx) = mpsc::channel(16_384);
        Ok(Self {
            inner: Arc::new(Inner {
                socket,
                routing: Mutex::new(Routing::new(id)),
                pending: Mutex::new(HashMap::new()),
                peers: Mutex::new(HashMap::new()),
                secrets: Mutex::new(TokenSecrets::new()),
                rates: Mutex::new(HashMap::new()),
                permits: Semaphore::new(config.max_inflight_queries),
                counters: Counters::default(),
                config,
                own_id: id,
                candidate_tx,
                sightings,
            }),
            candidates: Arc::new(tokio::sync::Mutex::new(candidate_rx)),
        })
    }

    pub fn stats(&self) -> DhtStats {
        let c = &self.inner.counters;
        DhtStats {
            packets_received: c.received.load(Ordering::Relaxed),
            queries_sent: c.sent.load(Ordering::Relaxed),
            replies_received: c.replies.load(Ordering::Relaxed),
            discovered_hashes: c.discovered.load(Ordering::Relaxed),
            sightings_dropped: c.dropped.load(Ordering::Relaxed),
            verified_nodes: self
                .inner
                .routing
                .lock()
                .expect("routing mutex poisoned")
                .count(),
        }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.socket.local_addr()
    }

    fn allowed(&self, addr: SocketAddrV4) -> bool {
        if self.inner.config.allow_private_addresses {
            addr.port() != 0 && !addr.ip().is_unspecified() && !addr.ip().is_multicast()
        } else {
            krpc::valid_public_address(SocketAddr::V4(addr))
        }
    }

    fn emit(&self, sighting: Sighting) {
        match self.inner.sightings.try_send(sighting) {
            Ok(()) => {
                self.inner
                    .counters
                    .discovered
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                self.inner.counters.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn observe_peer(&self, hash: Id, peer: SocketAddrV4) {
        if !self.allowed(peer) {
            return;
        }
        let mut peers = self.inner.peers.lock().expect("peers mutex poisoned");
        if peers.len() >= MAX_SEEN && !peers.contains_key(&hash) {
            if let Some(key) = peers.keys().next().copied() {
                peers.remove(&key);
            }
        }
        let set = peers.entry(hash).or_insert_with(|| PeerSet {
            updated: Instant::now(),
            peers: Vec::new(),
        });
        set.updated = Instant::now();
        if !set.peers.contains(&peer) {
            if set.peers.len() >= self.inner.config.max_peers_per_hash {
                set.peers.remove(0);
            }
            set.peers.push(peer);
        }
    }

    pub async fn run(&self, mut stop: watch::Receiver<bool>) -> io::Result<()> {
        let receiver = self.clone();
        let mut receive_task = tokio::spawn(async move { receiver.receive_loop().await });
        let bootstrap = self.resolve_bootstrap().await;
        let mut queue: VecDeque<_> = bootstrap.into_iter().collect();
        let mut seen: HashMap<SocketAddrV4, Instant> = HashMap::new();
        let mut tasks = JoinSet::new();
        let mut ticker = interval(self.inner.config.query_interval);
        let mut refresh = interval(Duration::from_secs(120));
        let mut rx = self.candidates.lock().await;

        loop {
            tokio::select! {
                _ = stop.changed() => {
                    if *stop.borrow() || stop.has_changed().is_err() { break; }
                }
                res = &mut receive_task => {
                    tasks.abort_all();
                    return res.map_err(io::Error::other)?;
                }
                Some(addr) = rx.recv() => {
                    if self.allowed(addr) && queue.len() < 32_768 { queue.push_back(addr); }
                }
                _ = refresh.tick() => {
                    let target: Id = rand::random();
                    for contact in self.inner.routing.lock().expect("routing mutex poisoned").nearest(&target, 512) {
                        if queue.len() >= 32_768 { break; }
                        queue.push_back(contact.addr);
                    }
                    if seen.len() > 80_000 {
                        seen.retain(|_, deadline| *deadline > Instant::now());
                    }
                    if queue.is_empty() {
                        for addr in self.resolve_bootstrap().await { queue.push_back(addr); }
                    }
                }
                _ = ticker.tick() => {
                    if tasks.len() >= self.inner.config.max_inflight_queries { continue; }
                    let Some(addr) = queue.pop_front() else { continue; };
                    if seen.get(&addr).is_some_and(|deadline| *deadline > Instant::now()) {
                        continue;
                    }
                    seen.insert(addr, Instant::now() + Duration::from_secs(300));
                    let node = self.clone();
                    tasks.spawn(async move {
                        let target: Id = rand::random();
                        // Sample responses yield hashes and nearby routing nodes.
                        // Older nodes may not support BEP 51: find_node is a
                        // standards-compliant fallback for traversal.
                        let sampled = node.query(addr, b"sample_infohashes", target).await;
                        if sampled.is_none() {
                            let _ = node.query(addr, b"find_node", target).await;
                        }
                        (addr, sampled.map(|response| response.interval).unwrap_or(Duration::from_secs(600)))
                    });
                }
                Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                    if let Ok((addr, interval)) = result {
                        // BEP 51 interval is 0..=21600. Respect remote refresh
                        // deadlines instead of repeatedly sampling stale sets.
                        let backoff = interval.max(Duration::from_secs(30));
                        seen.insert(addr, Instant::now() + backoff);
                    }
                }
            }
        }
        tasks.abort_all();
        receive_task.abort();
        Ok(())
    }

    async fn resolve_bootstrap(&self) -> Vec<SocketAddrV4> {
        let mut resolved = Vec::new();
        for hostname in &self.inner.config.bootstrap {
            if let Ok(Ok(addresses)) =
                timeout(Duration::from_secs(3), lookup_host(hostname.as_str())).await
            {
                for address in addresses {
                    if let SocketAddr::V4(v4) = address {
                        if self.allowed(v4) {
                            resolved.push(v4);
                        }
                    }
                }
            }
        }
        resolved
    }

    async fn query(&self, addr: SocketAddrV4, method: &[u8], target: Id) -> Option<Response> {
        if !self.allowed(addr) {
            return None;
        }
        let _permit = self.inner.permits.acquire().await.ok()?;
        let (send, receive) = oneshot::channel();
        let transaction;
        {
            let mut pending = self.inner.pending.lock().expect("pending mutex poisoned");
            let mut chosen = None;
            for _ in 0..20 {
                let candidate = rand::random::<u16>();
                if !pending.contains_key(&(addr, candidate)) {
                    chosen = Some(candidate);
                    break;
                }
            }
            transaction = chosen?;
            pending.insert((addr, transaction), send);
        }
        let payload = krpc::encode_query(&self.inner.own_id, transaction, method, &target);
        self.inner.counters.sent.fetch_add(1, Ordering::Relaxed);
        if self.inner.socket.send_to(&payload, addr).await.is_err() {
            self.inner
                .pending
                .lock()
                .expect("pending mutex poisoned")
                .remove(&(addr, transaction));
            return None;
        }
        let reply = timeout(self.inner.config.query_timeout, receive).await;
        self.inner
            .pending
            .lock()
            .expect("pending mutex poisoned")
            .remove(&(addr, transaction));
        let packet = reply.ok()?.ok()?;
        let data = bencode::decode(&packet, MAX_PACKET).ok()?;
        let Message::Response { data, .. } = krpc::parse(&data)? else {
            return None;
        };
        let id = krpc::id(data, b"id")?;
        if id == self.inner.own_id {
            return None;
        }
        self.inner
            .routing
            .lock()
            .expect("routing mutex poisoned")
            .insert_verified(NodeContact { id, addr });
        let nodes = data
            .get(b"nodes")
            .and_then(Value::bytes)
            .map(|bytes| krpc::decode_nodes(bytes, 128))
            .unwrap_or_default();
        for contact in &nodes {
            if self.allowed(contact.addr) {
                let _ = self.inner.candidate_tx.try_send(contact.addr);
            }
        }
        let peers = data
            .get(b"values")
            .map(|value| krpc::decode_peers(value, 48))
            .unwrap_or_default();
        let infohash = target;
        if method == b"get_peers" {
            for peer in &peers {
                self.observe_peer(infohash, *peer);
                self.emit(Sighting {
                    infohash,
                    peer: Some(*peer),
                    hint: Some(addr),
                });
            }
        }
        let samples: Vec<Id> = data
            .get(b"samples")
            .and_then(Value::bytes)
            .filter(|bytes| bytes.len() % 20 == 0 && bytes.len() <= 800)
            .map(|bytes| {
                bytes
                    .chunks_exact(20)
                    .filter_map(|chunk| chunk.try_into().ok())
                    .collect()
            })
            .unwrap_or_default();
        for hash in &samples {
            self.emit(Sighting {
                infohash: *hash,
                peer: None,
                hint: Some(addr),
            });
        }
        let interval_secs = data
            .get(b"interval")
            .and_then(Value::integer)
            .unwrap_or(300)
            .clamp(0, 21_600) as u64;
        Some(Response {
            nodes,
            peers,
            interval: Duration::from_secs(interval_secs),
        })
    }

    /// Iterative bounded get_peers walk; no unsolicited outbound announcements.
    pub async fn find_peers(&self, hash: Id, hints: &[SocketAddrV4]) -> Vec<SocketAddrV4> {
        let mut queue: VecDeque<_> = hints.iter().copied().filter(|a| self.allowed(*a)).collect();
        let nearest = self
            .inner
            .routing
            .lock()
            .expect("routing mutex poisoned")
            .nearest(&hash, 16);
        queue.extend(nearest.iter().map(|node| node.addr));
        let mut tried = HashSet::new();
        let mut peers = HashSet::new();
        let mut tasks = JoinSet::new();
        let budget = self.inner.config.lookup_budget.min(256);
        while tried.len() < budget || !tasks.is_empty() {
            while tasks.len() < 4 && tried.len() < budget {
                let Some(addr) = queue.pop_front() else {
                    break;
                };
                if !tried.insert(addr) {
                    continue;
                }
                let node = self.clone();
                tasks.spawn(async move { node.query(addr, b"get_peers", hash).await });
            }
            let Some(result) = tasks.join_next().await else {
                break;
            };
            if let Ok(Some(response)) = result {
                for peer in response.peers {
                    peers.insert(peer);
                }
                for contact in response.nodes {
                    if self.allowed(contact.addr)
                        && !tried.contains(&contact.addr)
                        && queue.len() < 1_000
                    {
                        queue.push_back(contact.addr);
                    }
                }
            }
        }
        peers.into_iter().take(96).collect()
    }

    async fn receive_loop(&self) -> io::Result<()> {
        let mut buffer = [0; MAX_PACKET];
        loop {
            let (size, from) = self.inner.socket.recv_from(&mut buffer).await?;
            let SocketAddr::V4(addr) = from else {
                continue;
            };
            if !self.allowed(addr) {
                continue;
            }
            self.inner.counters.received.fetch_add(1, Ordering::Relaxed);
            let Ok(root) = bencode::decode(&buffer[..size], MAX_PACKET) else {
                continue;
            };
            let Some(message) = krpc::parse(&root) else {
                continue;
            };
            match message {
                Message::Response { transaction, .. } | Message::Error { transaction } => {
                    if transaction.len() != 2 {
                        continue;
                    }
                    let tx = u16::from_be_bytes([transaction[0], transaction[1]]);
                    if let Some(sender) = self
                        .inner
                        .pending
                        .lock()
                        .expect("pending mutex poisoned")
                        .remove(&(addr, tx))
                    {
                        let _ = sender.send(buffer[..size].to_vec());
                        self.inner.counters.replies.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Message::Query {
                    transaction,
                    name,
                    args,
                } => {
                    if !self.allow_request(addr) {
                        continue;
                    }
                    let reply = self.answer(addr, transaction, name, args);
                    if let Some(packet) = reply {
                        if packet.len() <= 1400 {
                            let _ = self.inner.socket.send_to(&packet, addr).await;
                        }
                    }
                }
            }
        }
    }

    fn allow_request(&self, addr: SocketAddrV4) -> bool {
        let mut rates = self.inner.rates.lock().expect("rates mutex poisoned");
        if rates.len() > 20_000 {
            rates.retain(|_, entry| entry.since.elapsed() < Duration::from_secs(3));
            if rates.len() > 20_000 {
                rates.clear();
            }
        }
        let entry = rates.entry(*addr.ip()).or_insert(RateEntry {
            since: Instant::now(),
            count: 0,
        });
        if entry.since.elapsed() >= Duration::from_secs(1) {
            entry.since = Instant::now();
            entry.count = 0;
        }
        entry.count += 1;
        entry.count <= self.inner.config.inbound_queries_per_ip_second
    }

    fn answer(
        &self,
        source: SocketAddrV4,
        tx: &[u8],
        method: &[u8],
        args: &Value<'_>,
    ) -> Option<Vec<u8>> {
        let _remote_id = krpc::id(args, b"id")?;
        let id = &self.inner.own_id;
        let target = if method == b"get_peers" || method == b"announce_peer" {
            krpc::id(args, b"info_hash")
        } else {
            krpc::id(args, b"target")
        };
        let nodes = |target: &Id| {
            let contacts = self
                .inner
                .routing
                .lock()
                .expect("routing mutex poisoned")
                .nearest(target, 8);
            let mut packed = Vec::with_capacity(contacts.len() * 26);
            for item in &contacts {
                krpc::compact_node(item, &mut packed);
            }
            packed
        };
        match method {
            b"ping" => Some(krpc::encode_response(
                tx,
                Value::Dict(vec![(b"id", Value::Bytes(id))]),
            )),
            b"find_node" => {
                let target = target?;
                let compact = nodes(&target);
                Some(krpc::encode_response(
                    tx,
                    Value::Dict(vec![
                        (b"id", Value::Bytes(id)),
                        (b"nodes", Value::Bytes(&compact)),
                    ]),
                ))
            }
            b"get_peers" => {
                let target = target?;
                self.emit(Sighting {
                    infohash: target,
                    peer: None,
                    hint: Some(source),
                });
                let compact = nodes(&target);
                let token = self
                    .inner
                    .secrets
                    .lock()
                    .expect("token mutex poisoned")
                    .issue(*source.ip());
                let peers = self
                    .inner
                    .peers
                    .lock()
                    .expect("peers mutex poisoned")
                    .get(&target)
                    .filter(|set| set.updated.elapsed() < Duration::from_secs(45 * 60))
                    .map(|set| set.peers.clone())
                    .unwrap_or_default();
                let values: Vec<[u8; 6]> = peers
                    .iter()
                    .take(12)
                    .map(|p| krpc::compact_peer(*p))
                    .collect();
                let mut fields: Vec<(&[u8], Value<'_>)> = vec![
                    (b"id", Value::Bytes(id)),
                    (b"nodes", Value::Bytes(&compact)),
                    (b"token", Value::Bytes(&token)),
                ];
                if !values.is_empty() {
                    fields.push((
                        b"values",
                        Value::List(values.iter().map(|v| Value::Bytes(v)).collect()),
                    ));
                }
                Some(krpc::encode_response(tx, Value::Dict(fields)))
            }
            b"announce_peer" => {
                let target = target?;
                let token = args.get(b"token")?.bytes()?;
                if !self
                    .inner
                    .secrets
                    .lock()
                    .expect("token mutex poisoned")
                    .valid(*source.ip(), token)
                {
                    return Some(krpc::encode_error(tx, 203, b"Invalid token"));
                }
                let implicit = args
                    .get(b"implied_port")
                    .and_then(Value::integer)
                    .unwrap_or(0)
                    != 0;
                let port = if implicit {
                    source.port()
                } else {
                    u16::try_from(args.get(b"port")?.integer()?).ok()?
                };
                if port == 0 {
                    return Some(krpc::encode_error(tx, 203, b"Invalid port"));
                }
                let peer = SocketAddrV4::new(*source.ip(), port);
                self.observe_peer(target, peer);
                self.emit(Sighting {
                    infohash: target,
                    peer: Some(peer),
                    hint: Some(source),
                });
                Some(krpc::encode_response(
                    tx,
                    Value::Dict(vec![(b"id", Value::Bytes(id))]),
                ))
            }
            b"sample_infohashes" => {
                let target = target?;
                let compact = nodes(&target);
                let hashes = self.inner.peers.lock().expect("peers mutex poisoned");
                let mut samples = Vec::new();
                for (hash, _set) in hashes
                    .iter()
                    .filter(|(_, set)| set.updated.elapsed() < Duration::from_secs(45 * 60))
                    .take(MAX_SAMPLE)
                {
                    samples.extend_from_slice(hash);
                }
                let total = hashes.len() as i64;
                Some(krpc::encode_response(
                    tx,
                    Value::Dict(vec![
                        (b"id", Value::Bytes(id)),
                        (b"interval", Value::Integer(300)),
                        (b"nodes", Value::Bytes(&compact)),
                        (b"num", Value::Integer(total)),
                        (b"samples", Value::Bytes(&samples)),
                    ]),
                ))
            }
            _ => Some(krpc::encode_error(tx, 204, b"Method Unknown")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn token_is_scoped_to_ip_and_rotates() {
        let mut secrets = TokenSecrets::new();
        let ip = Ipv4Addr::new(8, 8, 8, 8);
        let token = secrets.issue(ip);
        assert!(secrets.valid(ip, &token));
        assert!(!secrets.valid(Ipv4Addr::new(8, 8, 4, 4), &token));
    }
}
