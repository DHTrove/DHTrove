//! Bounded XOR-distance routing cache. Only validated answering contacts
//! are promoted; unverified nodes received from strangers are probed first.

use crate::krpc::{closer, Id, NodeContact};
use std::net::SocketAddrV4;
use std::time::{Duration, Instant};

const K: usize = 8;
const LIFETIME: Duration = Duration::from_secs(25 * 60);

#[derive(Clone, Copy)]
struct Entry {
    contact: NodeContact,
    last_seen: Instant,
}

pub struct Routing {
    own: Id,
    buckets: Vec<Vec<Entry>>,
}

impl Routing {
    pub fn new(own: Id) -> Self {
        Self { own, buckets: (0..160).map(|_| Vec::new()).collect() }
    }

    fn bucket(&self, id: &Id) -> Option<usize> {
        if id == &self.own { return None; }
        let mut prefix = 0;
        for i in 0..20 {
            let distance = self.own[i] ^ id[i];
            if distance == 0 { prefix += 8; } else {
                return Some(prefix + distance.leading_zeros() as usize);
            }
        }
        None
    }

    pub fn insert_verified(&mut self, contact: NodeContact) {
        let Some(bucket) = self.bucket(&contact.id) else { return };
        let entries = &mut self.buckets[bucket];
        entries.retain(|entry| entry.last_seen.elapsed() < LIFETIME);
        if let Some(current) = entries.iter_mut().find(|entry| entry.contact.addr == contact.addr) {
            current.contact = contact;
            current.last_seen = Instant::now();
        } else if entries.len() < K {
            entries.push(Entry { contact, last_seen: Instant::now() });
        } else if let Some((index, _)) = entries.iter().enumerate().max_by_key(|(_, e)| e.last_seen.elapsed()) {
            // Allow replacement only of nodes that have not answered recently.
            if entries[index].last_seen.elapsed() > Duration::from_secs(15 * 60) {
                entries[index] = Entry { contact, last_seen: Instant::now() };
            }
        }
    }

    pub fn nearest(&self, target: &Id, max: usize) -> Vec<NodeContact> {
        let mut contacts: Vec<_> = self.buckets.iter().flatten()
            .filter(|entry| entry.last_seen.elapsed() < LIFETIME)
            .map(|entry| entry.contact).collect();
        contacts.sort_unstable_by(|a,b| closer(&a.id, &b.id, target));
        contacts.truncate(max);
        contacts
    }

    pub fn count(&self) -> usize {
        self.buckets.iter().map(Vec::len).sum()
    }

    pub fn addresses(&self, max: usize) -> Vec<SocketAddrV4> {
        self.buckets.iter().flatten()
            .filter(|entry| entry.last_seen.elapsed() < LIFETIME)
            .take(max).map(|entry| entry.contact.addr).collect()
    }
}
