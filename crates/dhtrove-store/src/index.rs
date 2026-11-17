//! Space-efficient sorted infohash index plus a mutable overlay.
//!
//! Frozen lookup entries are fixed-size byte arrays (40 bytes for v1, 52 for
//! v2) without per-key heap nodes, pointers, or HashMap bucket allocation.
//! Mutations are held in a small overlay; freeze() repacks the index.
//! Loading sorted checkpoints avoids building a giant temporary HashMap.

use crate::{Infohash, Location, StoreError};
use std::collections::HashMap;

const V1_SIZE: usize = 40;
const V2_SIZE: usize = 52;

#[derive(Default)]
pub(crate) struct HashIndex {
    v1: Vec<[u8; V1_SIZE]>,
    v2: Vec<[u8; V2_SIZE]>,
    delta: HashMap<Infohash, Option<Location>>,
    live: usize,
}

impl HashIndex {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn len(&self) -> usize {
        self.live
    }

    pub(crate) fn packed_bytes(&self) -> usize {
        self.v1.capacity() * V1_SIZE + self.v2.capacity() * V2_SIZE
    }

    pub(crate) fn delta_entries(&self) -> usize {
        self.delta.len()
    }

    pub(crate) fn get(&self, key: &Infohash) -> Option<Location> {
        if let Some(location) = self.delta.get(key) {
            return *location;
        }
        match key {
            Infohash::V1(hash) => self
                .v1
                .binary_search_by(|entry| entry[..20].cmp(hash.as_slice()))
                .ok()
                .map(|index| read_location(&self.v1[index][20..])),
            Infohash::V2(hash) => self
                .v2
                .binary_search_by(|entry| entry[..32].cmp(hash.as_slice()))
                .ok()
                .map(|index| read_location(&self.v2[index][32..])),
        }
    }

    pub(crate) fn contains_key(&self, key: &Infohash) -> bool {
        self.get(key).is_some()
    }

    pub(crate) fn insert(&mut self, key: Infohash, location: Location) -> Option<Location> {
        let old = self.get(&key);
        if old.is_none() {
            self.live += 1;
        }
        self.delta.insert(key, Some(location));
        old
    }

    pub(crate) fn remove(&mut self, key: &Infohash) -> Option<Location> {
        let old = self.get(key);
        if old.is_some() {
            self.live -= 1;
            self.delta.insert(key.clone(), None);
        }
        old
    }

    /// Iterate over live entries, applying inserted and deleted overlay keys.
    /// Entries are not sorted if the overlay is non-empty.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (Infohash, Location)> + '_ {
        let frozen_v1 = self.v1.iter().map(|entry| {
            let mut hash = [0; 20];
            hash.copy_from_slice(&entry[..20]);
            (Infohash::V1(hash), read_location(&entry[20..]))
        });
        let frozen_v2 = self.v2.iter().map(|entry| {
            let mut hash = [0; 32];
            hash.copy_from_slice(&entry[..32]);
            (Infohash::V2(hash), read_location(&entry[32..]))
        });
        frozen_v1
            .chain(frozen_v2)
            .filter(|(hash, _)| !self.delta.contains_key(hash))
            .chain(
                self.delta
                    .iter()
                    .filter_map(|(key, location)| location.map(|loc| (key.clone(), loc))),
            )
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = Location> + '_ {
        self.iter().map(|(_, location)| location)
    }

    /// Repack into sorted arrays. The temporary sort uses extra RAM; schedule
    /// this during maintenance rather than for every incoming torrent.
    pub(crate) fn freeze(&mut self) {
        if self.delta.is_empty() {
            self.v1.shrink_to_fit();
            self.v2.shrink_to_fit();
            return;
        }
        let mut rows: Vec<_> = self.iter().collect();
        rows.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let n_v1 = rows.iter().filter(|(key, _)| matches!(key, Infohash::V1(_))).count();
        let (mut v1, mut v2) = (
            Vec::with_capacity(n_v1),
            Vec::with_capacity(rows.len() - n_v1),
        );
        for (key, location) in rows {
            match key {
                Infohash::V1(hash) => v1.push(pack_v1(hash, location)),
                Infohash::V2(hash) => v2.push(pack_v2(hash, location)),
            }
        }
        self.v1 = v1;
        self.v2 = v2;
        self.delta.clear();
        debug_assert_eq!(self.live, self.v1.len() + self.v2.len());
    }

    /// Append already sorted checkpoint entries directly into packed arrays.
    pub(crate) fn push_sorted(
        &mut self,
        key: Infohash,
        location: Location,
    ) -> Result<(), StoreError> {
        if !self.delta.is_empty() {
            return Err(StoreError::Corrupt(
                "cannot load frozen index with overlay".into(),
            ));
        }
        match key {
            Infohash::V1(hash) => {
                if !self.v2.is_empty()
                    || self
                        .v1
                        .last()
                        .is_some_and(|last| last[..20].cmp(hash.as_slice()).is_ge())
                {
                    return Err(StoreError::Corrupt(
                        "unsorted or repeated v1 infohash".into(),
                    ));
                }
                self.v1.push(pack_v1(hash, location));
            }
            Infohash::V2(hash) => {
                if self
                    .v2
                    .last()
                    .is_some_and(|last| last[..32].cmp(hash.as_slice()).is_ge())
                {
                    return Err(StoreError::Corrupt(
                        "unsorted or repeated v2 infohash".into(),
                    ));
                }
                self.v2.push(pack_v2(hash, location));
            }
        }
        self.live += 1;
        Ok(())
    }
}

fn pack_v1(hash: [u8; 20], location: Location) -> [u8; V1_SIZE] {
    let mut entry = [0; V1_SIZE];
    entry[..20].copy_from_slice(&hash);
    write_location(&mut entry[20..], location);
    entry
}

fn pack_v2(hash: [u8; 32], location: Location) -> [u8; V2_SIZE] {
    let mut entry = [0; V2_SIZE];
    entry[..32].copy_from_slice(&hash);
    write_location(&mut entry[32..], location);
    entry
}

fn write_location(out: &mut [u8], location: Location) {
    out[..8].copy_from_slice(&location.segment.to_le_bytes());
    out[8..16].copy_from_slice(&location.offset.to_le_bytes());
    let ordinal =
        u32::try_from(location.ordinal).expect("ordinal bounded by the block codec record limit");
    out[16..20].copy_from_slice(&ordinal.to_le_bytes());
}

fn read_location(bytes: &[u8]) -> Location {
    Location {
        segment: u64::from_le_bytes(bytes[..8].try_into().expect("fixed segment ID")),
        offset: u64::from_le_bytes(bytes[8..16].try_into().expect("fixed frame offset")),
        ordinal: u32::from_le_bytes(bytes[16..20].try_into().expect("fixed ordinal")) as usize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(i: u64) -> Location {
        Location {
            segment: i / 300,
            offset: (i % 300) * 10 + 8,
            ordinal: (i % 50) as usize,
        }
    }

    #[test]
    fn overlay_insert_delete_and_reinsert() {
        let mut index = HashIndex::new();
        let a = Infohash::V1([1; 20]);
        let b = Infohash::V2([2; 32]);
        assert_eq!(index.insert(a.clone(), loc(1)), None);
        assert_eq!(index.insert(b.clone(), loc(2)), None);
        assert_eq!(index.len(), 2);
        index.freeze();
        assert_eq!(index.packed_bytes(), V1_SIZE + V2_SIZE);
        assert_eq!(index.get(&a), Some(loc(1)));
        assert_eq!(index.remove(&a), Some(loc(1)));
        assert_eq!(index.get(&a), None);
        assert_eq!(index.len(), 1);
        assert_eq!(index.insert(a.clone(), loc(3)), None);
        assert_eq!(index.get(&a), Some(loc(3)));
        index.freeze();
        assert_eq!(index.delta_entries(), 0);
        assert_eq!(index.get(&a), Some(loc(3)));
        assert_eq!(index.get(&b), Some(loc(2)));
    }

    #[test]
    fn checkpoint_streaming_sorted_keys() {
        let mut index = HashIndex::new();
        index.push_sorted(Infohash::V1([1; 20]), loc(1)).unwrap();
        index.push_sorted(Infohash::V1([3; 20]), loc(3)).unwrap();
        index.push_sorted(Infohash::V2([1; 32]), loc(4)).unwrap();
        assert_eq!(index.len(), 3);
        assert_eq!(index.get(&Infohash::V1([3; 20])), Some(loc(3)));
        assert!(index.push_sorted(Infohash::V1([4; 20]), loc(9)).is_err());
        assert!(index.push_sorted(Infohash::V2([1; 32]), loc(9)).is_err());
    }

    #[test]
    fn large_index_lookup_and_compact_representation() {
        let mut index = HashIndex::new();
        let count = 60_000u32;
        for i in (0..count).rev() {
            let mut hash = [0; 20];
            hash[..4].copy_from_slice(&i.to_be_bytes());
            index.insert(Infohash::V1(hash), loc(u64::from(i)));
        }
        index.freeze();
        assert_eq!(index.len(), count as usize);
        assert_eq!(index.packed_bytes(), count as usize * V1_SIZE);
        for i in [0, 1, 1234, 35_000, 59_999] {
            let mut hash = [0; 20];
            hash[..4].copy_from_slice(&(i as u32).to_be_bytes());
            assert_eq!(index.get(&Infohash::V1(hash)), Some(loc(i as u64)));
        }
        for i in 0..1_000u32 {
            let mut hash = [0; 20];
            hash[..4].copy_from_slice(&i.to_be_bytes());
            assert!(index.remove(&Infohash::V1(hash)).is_some());
        }
        assert_eq!(index.len(), 59_000);
        index.freeze();
        assert_eq!(index.packed_bytes(), 59_000 * V1_SIZE);
    }
}
