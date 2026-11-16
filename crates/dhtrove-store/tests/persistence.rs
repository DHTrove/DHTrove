use dhtrove_storage::{FileEntry, TorrentRecord};
use dhtrove_store::{Infohash, Store, StoreConfig, StoreError};
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use tempfile::tempdir;

fn record(number: u32) -> TorrentRecord {
    let mut sha1 = [0u8; 20];
    sha1[..4].copy_from_slice(&number.to_le_bytes());
    let mut sha256 = [0u8; 32];
    sha256[..4].copy_from_slice(&number.to_be_bytes());
    TorrentRecord {
        v1: Some(sha1),
        v2: (number % 2 == 0).then_some(sha256),
        name: format!("TV Show Season {:02}", number % 10).into_bytes(),
        files: (0..4)
            .map(|episode| FileEntry {
                path: format!("TV Show/Season {:02}/Episode {episode:02}.mkv", number % 10)
                    .into_bytes(),
                length: u64::from(number) * 1_000 + episode,
            })
            .collect(),
    }
}

#[test]
fn stores_and_reloads_v1_v2_hybrid_without_duplication() {
    let directory = tempdir().unwrap();
    let original = record(42);
    let v1 = Infohash::V1(original.v1.unwrap());
    let v2 = Infohash::V2(original.v2.unwrap());

    {
        let mut store = Store::open(directory.path(), StoreConfig::default()).unwrap();
        let result = store.append(&[original.clone(), original.clone()]).unwrap();
        assert_eq!(result.inserted, 1);
        assert_eq!(result.skipped_duplicates, 1);
        assert!(result.bytes_written > 0);
        assert_eq!(store.stats().torrent_records, 1);
        assert_eq!(store.stats().indexed_hashes, 2);
        assert_eq!(store.get(&v1).unwrap(), Some(original.clone()));
        assert_eq!(store.get(&v2).unwrap(), Some(original.clone()));
        assert_eq!(store.get(&Infohash::V1([255; 20])).unwrap(), None);
    }

    let mut store = Store::open(directory.path(), StoreConfig::default()).unwrap();
    assert_eq!(store.stats().torrent_records, 1);
    assert_eq!(store.get(&v2).unwrap(), Some(original.clone()));
    let result = store.append(&[original]).unwrap();
    assert_eq!(result.inserted, 0);
    assert_eq!(result.bytes_written, 0);
}

#[test]
fn rotates_segments_and_rebuilds_index() {
    let directory = tempdir().unwrap();
    let config = StoreConfig {
        max_segment_bytes: 400,
        max_disk_bytes: None,
    };
    {
        let mut store = Store::open(directory.path(), config).unwrap();
        for i in 0..25 {
            assert_eq!(store.append(&[record(i)]).unwrap().inserted, 1);
        }
        assert!(store.stats().segments > 1);
    }
    let store = Store::open(directory.path(), config).unwrap();
    assert_eq!(store.stats().torrent_records, 25);
    for i in 0..25 {
        let expected = record(i);
        assert_eq!(
            store.get(&Infohash::V1(expected.v1.unwrap())).unwrap(),
            Some(expected)
        );
    }
}

#[test]
fn truncates_incomplete_active_tail_without_losing_committed_frames() {
    let directory = tempdir().unwrap();
    let committed_len;
    {
        let mut store = Store::open(directory.path(), StoreConfig::default()).unwrap();
        store.append(&[record(1)]).unwrap();
        committed_len = store.stats().disk_bytes;
    }
    let path = directory.path().join("segment-0000000000000000.dhs");
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(&100_u32.to_le_bytes()).unwrap();
    file.write_all(&123_u32.to_le_bytes()).unwrap();
    file.write_all(&[1, 2, 3]).unwrap();
    file.sync_all().unwrap();

    let store = Store::open(directory.path(), StoreConfig::default()).unwrap();
    assert_eq!(store.stats().torrent_records, 1);
    assert_eq!(store.stats().disk_bytes, committed_len);
    assert!(store
        .get(&Infohash::V1(record(1).v1.unwrap()))
        .unwrap()
        .is_some());
}

#[test]
fn checksums_prevent_silent_corruption() {
    let directory = tempdir().unwrap();
    {
        let mut store = Store::open(directory.path(), StoreConfig::default()).unwrap();
        store.append(&[record(7)]).unwrap();
    }
    let path = directory.path().join("segment-0000000000000000.dhs");
    let mut file = OpenOptions::new().write(true).open(path).unwrap();
    file.seek(SeekFrom::Start(16)).unwrap();
    file.write_all(&[0xff]).unwrap();
    file.sync_all().unwrap();

    let result = Store::open(directory.path(), StoreConfig::default());
    assert!(matches!(result, Err(StoreError::Corrupt(_))));
}

#[test]
fn quota_rejects_new_writes_and_preserves_data() {
    let directory = tempdir().unwrap();
    let used;
    {
        let mut store = Store::open(directory.path(), StoreConfig::default()).unwrap();
        store.append(&[record(1)]).unwrap();
        used = store.stats().disk_bytes;
    }
    let config = StoreConfig {
        max_segment_bytes: 32 * 1024 * 1024,
        max_disk_bytes: Some(used),
    };
    let mut store = Store::open(directory.path(), config).unwrap();
    assert!(matches!(
        store.append(&[record(2)]),
        Err(StoreError::Quota { .. })
    ));
    assert_eq!(store.stats().torrent_records, 1);
    assert_eq!(store.stats().disk_bytes, used);
}

#[test]
fn accepts_non_utf8_paths_and_rejects_invalid_records() {
    let directory = tempdir().unwrap();
    let mut store = Store::open(directory.path(), StoreConfig::default()).unwrap();
    let mut unusual = record(8);
    unusual.name = vec![0xff, 0, b'a'];
    unusual.files[0].path = vec![0xf0, 0x80, 0, b'/', b'x'];
    store.append(&[unusual.clone()]).unwrap();
    assert_eq!(
        store.get(&Infohash::V1(unusual.v1.unwrap())).unwrap(),
        Some(unusual)
    );

    let mut invalid = record(9);
    invalid.v1 = None;
    invalid.v2 = None;
    assert!(matches!(
        store.append(&[invalid]),
        Err(StoreError::Invalid(_))
    ));
}

#[test]
fn prevents_simultaneous_writers() {
    let directory = tempdir().unwrap();
    let _first = Store::open(directory.path(), StoreConfig::default()).unwrap();
    assert!(Store::open(directory.path(), StoreConfig::default()).is_err());
}

#[test]
fn does_not_erase_corrupt_closed_segments() {
    let directory = tempdir().unwrap();
    let config = StoreConfig {
        max_segment_bytes: 400,
        max_disk_bytes: None,
    };
    {
        let mut store = Store::open(directory.path(), config).unwrap();
        for i in 0..10 {
            store.append(&[record(i)]).unwrap();
        }
        assert!(store.stats().segments > 1);
    }
    let first = directory.path().join("segment-0000000000000000.dhs");
    let mut file = OpenOptions::new().append(true).open(first).unwrap();
    file.write_all(&[1, 2, 3]).unwrap();
    file.sync_all().unwrap();
    assert!(matches!(
        Store::open(directory.path(), config),
        Err(StoreError::Corrupt(_))
    ));
}
