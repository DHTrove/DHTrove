use dhtrove_storage::{FileEntry, TorrentRecord};
use dhtrove_store::{Infohash, Store, StoreConfig, StoreError};
use std::fs::{self, OpenOptions};
use std::io::Write;
use tempfile::tempdir;

fn torrent(i: u32) -> TorrentRecord {
    let mut v1 = [0u8; 20];
    v1[..4].copy_from_slice(&i.to_le_bytes());
    let mut v2 = [0u8; 32];
    v2[..4].copy_from_slice(&i.to_be_bytes());
    TorrentRecord {
        v1: Some(v1),
        v2: (i % 2 == 0).then_some(v2),
        name: format!("Show Season {i:04} WEB-DL").into_bytes(),
        files: (0..5)
            .map(|episode| FileEntry {
                path: format!("Show/Season {i:04}/Episode {episode:02}.mkv").into_bytes(),
                length: u64::from(i) * 1000 + episode,
            })
            .collect(),
    }
}

fn hash(i: u32) -> Infohash {
    Infohash::V1(torrent(i).v1.unwrap())
}

#[test]
fn compaction_physically_reclaims_tombstoned_blocks() {
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path(), StoreConfig::default()).unwrap();
    for i in 0..100 {
        store.append(&[torrent(i)]).unwrap();
    }
    store.checkpoint().unwrap();
    for i in 0..80 {
        assert!(store.delete(&hash(i)).unwrap());
    }
    let before = store.stats();
    let outcome = store.compact().unwrap();
    assert!(outcome.performed);
    assert!(outcome.new_bytes < outcome.old_bytes);
    assert!(outcome.reclaimed_bytes > 0);
    assert_eq!(outcome.records_kept, 20);
    assert!(store.stats().disk_bytes < before.disk_bytes);
    assert_eq!(store.stats().checkpoint_bytes, 0);

    for i in 0..100 {
        assert_eq!(store.get(&hash(i)).unwrap().is_some(), i >= 80);
    }
    store.checkpoint().unwrap();
    drop(store);
    let store = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert!(store.stats().checkpoint_loaded_on_open);
    assert_eq!(store.stats().torrent_records, 20);
    for i in 80..100 {
        assert_eq!(store.get(&hash(i)).unwrap(), Some(torrent(i)));
    }
}

#[test]
fn old_checkpoint_cannot_resurrect_deleted_data_after_switch() {
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path(), StoreConfig::default()).unwrap();
    store.append(&[torrent(1), torrent(2), torrent(3)]).unwrap();
    store.checkpoint().unwrap();
    store.delete(&hash(1)).unwrap();
    store.delete(&hash(2)).unwrap();
    assert!(store.compact().unwrap().performed);
    drop(store);
    let store = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert_eq!(store.stats().torrent_records, 1);
    assert!(store.get(&hash(1)).unwrap().is_none());
    assert!(store.get(&hash(2)).unwrap().is_none());
    assert!(store.get(&hash(3)).unwrap().is_some());
}

#[test]
fn manifest_ignores_unpublished_segment_files() {
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path(), StoreConfig::default()).unwrap();
    store.append(&[torrent(8)]).unwrap();
    // Force creation of a manifest by rolling over to another segment.
    drop(store);
    let config = StoreConfig {
        max_segment_bytes: 450,
        max_disk_bytes: None,
    };
    let mut store = Store::open(dir.path(), config).unwrap();
    for n in 9..20 {
        store.append(&[torrent(n)]).unwrap();
    }
    assert!(dir.path().join("segments.manifest").exists());
    let active_count = store.stats().torrent_records;
    drop(store);

    // Represents a crash after copying new files but before atomic publish.
    let orphan = dir.path().join("segment-000000000000ffff.dhs");
    let existing = dir.path().join("segment-0000000000000000.dhs");
    fs::copy(existing, &orphan).unwrap();

    let store = Store::open(dir.path(), config).unwrap();
    assert_eq!(store.stats().torrent_records, active_count);
    assert!(!orphan.exists());
    assert!(store.get(&hash(8)).unwrap().is_some());
}

#[test]
fn corrupt_manifest_is_never_ignored() {
    let dir = tempdir().unwrap();
    let config = StoreConfig {
        max_segment_bytes: 450,
        max_disk_bytes: None,
    };
    {
        let mut store = Store::open(dir.path(), config).unwrap();
        for i in 0..12 {
            store.append(&[torrent(i)]).unwrap();
        }
    }
    let path = dir.path().join("segments.manifest");
    assert!(path.exists());
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(&[42]).unwrap();
    file.sync_all().unwrap();
    assert!(matches!(
        Store::open(dir.path(), config),
        Err(StoreError::Corrupt(_))
    ));
}

#[test]
fn compaction_requires_temporary_capacity_when_quota_set() {
    let dir = tempdir().unwrap();
    let consumed;
    {
        let mut store = Store::open(dir.path(), StoreConfig::default()).unwrap();
        for i in 0..30 {
            store.append(&[torrent(i)]).unwrap();
        }
        for i in 0..25 {
            store.delete(&hash(i)).unwrap();
        }
        consumed = store.stats().disk_bytes;
    }
    let config = StoreConfig {
        max_segment_bytes: StoreConfig::default().max_segment_bytes,
        max_disk_bytes: Some(consumed),
    };
    let mut store = Store::open(dir.path(), config).unwrap();
    assert!(matches!(store.compact(), Err(StoreError::Quota { .. })));
    assert_eq!(store.stats().torrent_records, 5);
    assert!(store.get(&hash(29)).unwrap().is_some());
    drop(store);
    assert_eq!(
        Store::open(dir.path(), StoreConfig::default())
            .unwrap()
            .stats()
            .torrent_records,
        5
    );
}

#[test]
fn stage_left_by_crash_is_ignored_and_cleaned() {
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path(), StoreConfig::default()).unwrap();
    store.append(&[torrent(1)]).unwrap();
    drop(store);
    let stage = dir.path().join(".dhtrove-compacting");
    fs::create_dir(&stage).unwrap();
    fs::write(stage.join("segment-0000000000000001.dhs"), b"partial").unwrap();
    let store = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert!(!stage.exists());
    assert_eq!(store.get(&hash(1)).unwrap(), Some(torrent(1)));
}

#[test]
fn appends_after_compaction_work_with_manifest_rollover() {
    let dir = tempdir().unwrap();
    let config = StoreConfig {
        max_segment_bytes: 450,
        max_disk_bytes: None,
    };
    let mut store = Store::open(dir.path(), config).unwrap();
    for i in 0..25 {
        store.append(&[torrent(i)]).unwrap();
    }
    for i in 0..23 {
        store.delete(&hash(i)).unwrap();
    }
    assert!(store.compact().unwrap().performed);
    for i in 25..50 {
        store.append(&[torrent(i)]).unwrap();
    }
    store.checkpoint().unwrap();
    drop(store);

    let store = Store::open(dir.path(), config).unwrap();
    assert!(store.stats().checkpoint_loaded_on_open);
    assert_eq!(store.stats().torrent_records, 27);
    assert!(store.get(&hash(1)).unwrap().is_none());
    for i in 23..50 {
        assert_eq!(store.get(&hash(i)).unwrap(), Some(torrent(i)));
    }
}
