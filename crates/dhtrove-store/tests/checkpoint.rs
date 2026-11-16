use dhtrove_storage::{FileEntry, TorrentRecord};
use dhtrove_store::{Infohash, Store, StoreConfig, StoreError};
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use tempfile::tempdir;

fn torrent(i: u32) -> TorrentRecord {
    let mut hash = [0; 20];
    hash[..4].copy_from_slice(&i.to_le_bytes());
    TorrentRecord {
        v1: Some(hash),
        v2: None,
        name: format!("Season {i:02}").into_bytes(),
        files: vec![FileEntry {
            path: format!("Series/Season {i:02}/Episode 01.mkv").into_bytes(),
            length: u64::from(i) * 1_000_000,
        }],
    }
}

fn lookup(store: &Store, i: u32) -> bool {
    store
        .get(&Infohash::V1(torrent(i).v1.unwrap()))
        .unwrap()
        .is_some()
}

#[test]
fn checkpoint_is_compressed_and_reloadable() {
    let dir = tempdir().unwrap();
    let checkpoint_size;
    {
        let mut store = Store::open(dir.path(), StoreConfig::default()).unwrap();
        store
            .append(&(0..500).map(torrent).collect::<Vec<_>>())
            .unwrap();
        checkpoint_size = store.checkpoint().unwrap();
        assert!(checkpoint_size > 0);
        assert_eq!(checkpoint_size, store.stats().checkpoint_bytes);
        assert!(!store.stats().checkpoint_loaded_on_open);
    }
    assert_eq!(
        fs::metadata(dir.path().join("hash-index.zst"))
            .unwrap()
            .len(),
        checkpoint_size
    );
    let store = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert!(store.stats().checkpoint_loaded_on_open);
    assert_eq!(store.stats().torrent_records, 500);
    assert!(lookup(&store, 0));
    assert!(lookup(&store, 499));
}

#[test]
fn replays_only_appends_after_the_last_checkpoint() {
    let dir = tempdir().unwrap();
    {
        let mut store = Store::open(dir.path(), StoreConfig::default()).unwrap();
        store.append(&[torrent(1)]).unwrap();
        store.checkpoint().unwrap();
        store.append(&[torrent(2), torrent(3)]).unwrap();
    }

    let mut store = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert!(store.stats().checkpoint_loaded_on_open);
    assert_eq!(store.stats().torrent_records, 3);
    for i in 1..4 {
        assert!(lookup(&store, i));
    }
    store.checkpoint().unwrap();
    drop(store);
    let store = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert!(store.stats().checkpoint_loaded_on_open);
    assert_eq!(store.stats().torrent_records, 3);
}

#[test]
fn malformed_checkpoint_falls_back_to_full_segment_scan() {
    let dir = tempdir().unwrap();
    {
        let mut store = Store::open(dir.path(), StoreConfig::default()).unwrap();
        store.append(&[torrent(4), torrent(5)]).unwrap();
        store.checkpoint().unwrap();
    }
    let index_file = dir.path().join("hash-index.zst");
    let mut file = OpenOptions::new().write(true).open(&index_file).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(b"BAD!").unwrap();
    file.sync_all().unwrap();

    let store = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert!(!store.stats().checkpoint_loaded_on_open);
    assert_eq!(store.stats().torrent_records, 2);
    assert!(lookup(&store, 4));
    assert!(lookup(&store, 5));
    assert!(!index_file.exists());
}

#[test]
fn incomplete_tail_is_recovered_while_replaying_checkpoint() {
    let dir = tempdir().unwrap();
    let correct_len;
    {
        let mut store = Store::open(dir.path(), StoreConfig::default()).unwrap();
        store.append(&[torrent(6)]).unwrap();
        store.checkpoint().unwrap();
        store.append(&[torrent(7)]).unwrap();
        correct_len = fs::metadata(dir.path().join("segment-0000000000000000.dhs"))
            .unwrap()
            .len();
    }
    let path = dir.path().join("segment-0000000000000000.dhs");
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(&100_u32.to_le_bytes()).unwrap();
    file.write_all(&0_u32.to_le_bytes()).unwrap();
    file.write_all(&[1, 2, 3]).unwrap();
    file.sync_all().unwrap();

    let store = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert!(store.stats().checkpoint_loaded_on_open);
    assert_eq!(store.stats().torrent_records, 2);
    assert!(lookup(&store, 6));
    assert!(lookup(&store, 7));
    assert_eq!(fs::metadata(path).unwrap().len(), correct_len);
}

#[test]
fn checkpoint_respects_final_logical_disk_quota() {
    let dir = tempdir().unwrap();
    let segment_bytes;
    {
        let mut store = Store::open(dir.path(), StoreConfig::default()).unwrap();
        store.append(&[torrent(8)]).unwrap();
        segment_bytes = store.stats().disk_bytes;
    }
    let config = StoreConfig {
        max_segment_bytes: StoreConfig::default().max_segment_bytes,
        max_disk_bytes: Some(segment_bytes),
    };
    let mut store = Store::open(dir.path(), config).unwrap();
    assert!(matches!(store.checkpoint(), Err(StoreError::Quota { .. })));
    assert!(!dir.path().join("hash-index.zst").exists());
    assert!(!dir.path().join("hash-index.zst.tmp").exists());
    assert_eq!(store.stats().disk_bytes, segment_bytes);
}

#[test]
fn snapshot_remains_usable_across_segment_rollover() {
    let dir = tempdir().unwrap();
    let config = StoreConfig {
        max_segment_bytes: 350,
        max_disk_bytes: None,
    };
    {
        let mut store = Store::open(dir.path(), config).unwrap();
        store.append(&[torrent(9)]).unwrap();
        store.checkpoint().unwrap();
        for i in 10..35 {
            store.append(&[torrent(i)]).unwrap();
        }
        assert!(store.stats().segments > 1);
    }
    let store = Store::open(dir.path(), config).unwrap();
    assert!(store.stats().checkpoint_loaded_on_open);
    assert_eq!(store.stats().torrent_records, 26);
    for i in 9..35 {
        assert!(lookup(&store, i));
    }
}
