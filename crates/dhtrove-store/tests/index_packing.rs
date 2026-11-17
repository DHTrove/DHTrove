use dhtrove_storage::{FileEntry, TorrentRecord};
use dhtrove_store::{Infohash, Store, StoreConfig};
use tempfile::tempdir;

fn record(i: u32) -> TorrentRecord {
    let mut v1 = [0u8; 20];
    v1[..4].copy_from_slice(&i.to_be_bytes());
    let mut v2 = [0u8; 32];
    v2[..4].copy_from_slice(&i.to_le_bytes());
    TorrentRecord {
        v1: Some(v1),
        v2: (i % 4 == 0).then_some(v2),
        name: format!("Series Season {i}").into_bytes(),
        files: vec![FileEntry {
            path: format!("Series/S{i:04}/Episode01.mkv").into_bytes(),
            length: u64::from(i) * 9,
        }],
    }
}

#[test]
fn checkpoint_restores_packed_index_and_replays_mutations() {
    let dir = tempdir().unwrap();
    let count = 2_000u32;
    {
        let mut store = Store::open(dir.path(), StoreConfig::default()).unwrap();
        for slice in (0..count).collect::<Vec<_>>().chunks(40) {
            let batch: Vec<_> = slice.iter().copied().map(record).collect();
            store.append(&batch).unwrap();
        }
        let stats = store.stats();
        assert_eq!(stats.torrent_records, count as u64);
        assert_eq!(stats.indexed_hashes, 2_500);
        assert!(stats.index_delta_entries > 0);
        let size = store.checkpoint().unwrap();
        assert!(size > 0);
        let stats = store.stats();
        assert_eq!(stats.index_delta_entries, 0);
        assert!(stats.packed_index_bytes >= 2_000 * 40 + 500 * 52);
    }
    {
        let mut store = Store::open(dir.path(), StoreConfig::default()).unwrap();
        assert!(store.stats().checkpoint_loaded_on_open);
        assert_eq!(store.stats().index_delta_entries, 0);
        for i in [0, 3, 17, 511, 1_024, 1_999] {
            let row = record(i);
            assert_eq!(store.get(&Infohash::V1(row.v1.unwrap())).unwrap(), Some(row.clone()));
            if let Some(hash) = row.v2 {
                assert_eq!(store.get(&Infohash::V2(hash)).unwrap(), Some(row));
            }
        }
        for i in [0, 4, 201, 1_999] {
            store.delete(&Infohash::V1(record(i).v1.unwrap())).unwrap();
        }
        store.append(&[record(2_001), record(2_002)]).unwrap();
        assert_eq!(store.stats().torrent_records, count as u64 - 2);
        assert!(store.stats().index_delta_entries >= 4);
        store.checkpoint().unwrap();
        assert_eq!(store.stats().index_delta_entries, 0);
    }
    let store = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert!(store.stats().checkpoint_loaded_on_open);
    assert_eq!(store.stats().torrent_records, count as u64 - 2);
    for i in [0, 4, 201, 1_999] {
        let row = record(i);
        assert!(store.get(&Infohash::V1(row.v1.unwrap())).unwrap().is_none());
        if let Some(v2) = row.v2 {
            assert!(store.get(&Infohash::V2(v2)).unwrap().is_none());
        }
    }
    for i in [2_001, 2_002] {
        let row = record(i);
        assert_eq!(store.get(&Infohash::V1(row.v1.unwrap())).unwrap(), Some(row));
    }
}

#[test]
fn large_replay_without_checkpoint_is_packed_on_open() {
    let dir = tempdir().unwrap();
    {
        let mut store = Store::open(dir.path(), StoreConfig::default()).unwrap();
        for i in 0..500 {
            store.append(&[record(i)]).unwrap();
        }
    }
    let store = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert!(!store.stats().checkpoint_loaded_on_open);
    assert_eq!(store.stats().index_delta_entries, 0);
    assert_eq!(store.stats().torrent_records, 500);
    assert!(store.stats().packed_index_bytes > 0);
}

#[test]
fn compaction_keeps_index_packed_after_deletion() {
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path(), StoreConfig::default()).unwrap();
    for i in 0..80 {
        store.append(&[record(i)]).unwrap();
    }
    store.checkpoint().unwrap();
    for i in 0..60 {
        store.delete(&Infohash::V1(record(i).v1.unwrap())).unwrap();
    }
    assert!(store.compact().unwrap().performed);
    assert_eq!(store.stats().index_delta_entries, 0);
    assert_eq!(store.stats().torrent_records, 20);
    assert!(store.stats().packed_index_bytes >= 20 * 40 + 5 * 52);
}
