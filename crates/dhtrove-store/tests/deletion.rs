use dhtrove_storage::{FileEntry, TorrentRecord};
use dhtrove_store::{Infohash, Store, StoreConfig, StoreError};
use std::fs::OpenOptions;
use std::io::Write;
use tempfile::tempdir;

fn record(number: u32) -> TorrentRecord {
    let mut v1 = [0u8; 20];
    v1[..4].copy_from_slice(&number.to_le_bytes());
    let mut v2 = [0u8; 32];
    v2[..4].copy_from_slice(&number.to_be_bytes());
    TorrentRecord {
        v1: Some(v1),
        v2: (number % 2 == 0).then_some(v2),
        name: format!("Season {number:03}").into_bytes(),
        files: vec![FileEntry {
            path: format!("Series/S{number:03}/episode.mkv").into_bytes(),
            length: u64::from(number) * 1000,
        }],
    }
}

fn key(number: u32) -> Infohash {
    Infohash::V1(record(number).v1.unwrap())
}

#[test]
fn hybrid_deletion_removes_both_keys_durably() {
    let dir = tempdir().unwrap();
    let hybrid = record(2);
    let v1 = key(2);
    let v2 = Infohash::V2(hybrid.v2.unwrap());
    {
        let mut store = Store::open(dir.path(), StoreConfig::default()).unwrap();
        store.append(&[hybrid]).unwrap();
        assert_eq!(store.stats().indexed_hashes, 2);
        assert!(store.delete(&v2).unwrap());
        assert!(!store.delete(&v1).unwrap());
        assert_eq!(store.stats().torrent_records, 0);
        assert_eq!(store.stats().indexed_hashes, 0);
    }
    let store = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert!(store.get(&v1).unwrap().is_none());
    assert!(store.get(&v2).unwrap().is_none());
    assert_eq!(store.stats().torrent_records, 0);
}

#[test]
fn delete_then_reinsert_is_visible_after_replay() {
    let dir = tempdir().unwrap();
    {
        let mut store = Store::open(dir.path(), StoreConfig::default()).unwrap();
        store.append(&[record(1)]).unwrap();
        assert!(store.delete(&key(1)).unwrap());
        store.append(&[record(1)]).unwrap();
    }
    let store = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert_eq!(store.stats().torrent_records, 1);
    assert_eq!(store.get(&key(1)).unwrap(), Some(record(1)));
}

#[test]
fn tombstones_replay_after_checkpoint_and_segment_rollover() {
    let dir = tempdir().unwrap();
    let config = StoreConfig {
        max_segment_bytes: 400,
        max_disk_bytes: None,
    };
    {
        let mut store = Store::open(dir.path(), config).unwrap();
        for n in 0..20 {
            store.append(&[record(n)]).unwrap();
        }
        store.checkpoint().unwrap();
        for n in [0, 3, 8, 14, 18] {
            assert!(store.delete(&key(n)).unwrap());
        }
        assert!(store.stats().segments > 1);
    }
    let mut store = Store::open(dir.path(), config).unwrap();
    assert!(store.stats().checkpoint_loaded_on_open);
    assert_eq!(store.stats().torrent_records, 15);
    for n in 0..20 {
        assert_eq!(
            store.get(&key(n)).unwrap().is_some(),
            ![0, 3, 8, 14, 18].contains(&n)
        );
    }
    store.checkpoint().unwrap();
    drop(store);
    let store = Store::open(dir.path(), config).unwrap();
    assert!(store.stats().checkpoint_loaded_on_open);
    assert_eq!(store.stats().torrent_records, 15);
}

#[test]
fn failed_quota_does_not_remove_logical_record() {
    let dir = tempdir().unwrap();
    let consumed;
    {
        let mut store = Store::open(dir.path(), StoreConfig::default()).unwrap();
        store.append(&[record(4)]).unwrap();
        consumed = store.stats().disk_bytes;
    }
    let mut store = Store::open(
        dir.path(),
        StoreConfig {
            max_segment_bytes: StoreConfig::default().max_segment_bytes,
            max_disk_bytes: Some(consumed),
        },
    )
    .unwrap();
    assert!(matches!(
        store.delete(&key(4)),
        Err(StoreError::Quota { .. })
    ));
    assert_eq!(store.get(&key(4)).unwrap(), Some(record(4)));
    assert_eq!(store.stats().torrent_records, 1);
}

#[test]
fn partial_tombstone_tail_is_discarded_safely() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("segment-0000000000000000.dhs");
    {
        let mut store = Store::open(dir.path(), StoreConfig::default()).unwrap();
        store.append(&[record(7)]).unwrap();
    }
    let original = std::fs::metadata(&path).unwrap().len();
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(&35_u32.to_le_bytes()).unwrap();
    file.write_all(&0_u32.to_le_bytes()).unwrap();
    file.write_all(b"DHTD").unwrap();
    file.sync_all().unwrap();
    let store = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert_eq!(store.stats().torrent_records, 1);
    assert!(store.get(&key(7)).unwrap().is_some());
    assert_eq!(std::fs::metadata(path).unwrap().len(), original);
}

#[test]
fn deletion_preserves_other_records_in_same_compressed_frame() {
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path(), StoreConfig::default()).unwrap();
    let batch: Vec<_> = (0..12).map(record).collect();
    store.append(&batch).unwrap();
    assert!(store.delete(&key(5)).unwrap());
    for n in 0..12 {
        assert_eq!(store.get(&key(n)).unwrap().is_some(), n != 5);
    }
    drop(store);
    let store = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert_eq!(store.stats().torrent_records, 11);
    assert_eq!(store.get(&key(9)).unwrap(), Some(record(9)));
}
