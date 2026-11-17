//! Lossless v1 / BEP 52 v2 file tree extraction from the exact info bytes.
//! Never hash a re-encoded dictionary; the original bencoded bytes are the
//! source of truth for infohash verification.

use dhtrove_dht::bencode::{decode, Value};
use dhtrove_storage::{FileEntry, TorrentRecord, MAX_FILES, MAX_PATH_BYTES};
use sha1::{Digest, Sha1};
use sha2::Sha256;

#[derive(Debug)]
pub enum Error {
    Decode,
    Invalid(&'static str),
    HashMismatch,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid torrent info: {self:?}")
    }
}
impl std::error::Error for Error {}

pub fn decode_info(bytes: &[u8], observed: &[u8; 20]) -> Result<TorrentRecord, Error> {
    let info = decode(bytes, bytes.len()).map_err(|_| Error::Decode)?;
    let name = info
        .get(b"name.utf-8")
        .or_else(|| info.get(b"name"))
        .and_then(Value::bytes)
        .ok_or(Error::Invalid("missing name"))?;
    if name.len() > MAX_PATH_BYTES {
        return Err(Error::Invalid("torrent name too long"));
    }

    let version = info.get(b"meta version").and_then(Value::integer);
    if version.is_some_and(|value| value != 2) {
        return Err(Error::Invalid("unsupported torrent metainfo version"));
    }
    let has_v2 = version == Some(2);
    let has_v1 = !has_v2 || info.get(b"pieces").and_then(Value::bytes).is_some();
    let hash1: [u8; 20] = Sha1::digest(bytes).into();
    let hash2: [u8; 32] = Sha256::digest(bytes).into();
    if (!has_v1 || &hash1 != observed) && (!has_v2 || &hash2[..20] != observed) {
        return Err(Error::HashMismatch);
    }
    if has_v2 {
        // BEP 52 requires canonical bencoding for the SHA-256 infohash.
        check_sorted(&info)?;
    }

    let files = if has_v1 {
        parse_v1(&info, name)?
    } else {
        parse_v2(&info)?
    };
    if files.len() > MAX_FILES {
        return Err(Error::Invalid("too many torrent files"));
    }
    Ok(TorrentRecord {
        v1: has_v1.then_some(hash1),
        v2: has_v2.then_some(hash2),
        name: name.to_vec(),
        files,
    })
}

fn check_sorted(value: &Value<'_>) -> Result<(), Error> {
    match value {
        Value::Dict(entries) => {
            if entries.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
                return Err(Error::Invalid("noncanonical BEP 52 dictionary ordering"));
            }
            for (_, entry) in entries {
                check_sorted(entry)?;
            }
        }
        Value::List(list) => {
            for item in list {
                check_sorted(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn positive(value: Option<i64>) -> Result<u64, Error> {
    let number = value.ok_or(Error::Invalid("missing file length"))?;
    u64::try_from(number).map_err(|_| Error::Invalid("negative file length"))
}

fn path_component(data: &[u8]) -> Result<(), Error> {
    if data.is_empty()
        || data == b"."
        || data == b".."
        || data.contains(&b'/')
        || data.contains(&b'\\')
        || data.contains(&0)
    {
        return Err(Error::Invalid("unsafe file path component"));
    }
    Ok(())
}

fn append_path(base: &[u8], component: &[u8]) -> Result<Vec<u8>, Error> {
    path_component(component)?;
    let mut out = base.to_vec();
    if !out.is_empty() {
        out.push(b'/');
    }
    out.extend_from_slice(component);
    if out.len() > MAX_PATH_BYTES {
        return Err(Error::Invalid("file path too long"));
    }
    Ok(out)
}

fn parse_v1(info: &Value<'_>, name: &[u8]) -> Result<Vec<FileEntry>, Error> {
    if let Some(files) = info.get(b"files").and_then(Value::list) {
        if files.len() > MAX_FILES {
            return Err(Error::Invalid("too many files"));
        }
        let mut out = Vec::with_capacity(files.len());
        for file in files {
            let length = positive(file.get(b"length").and_then(Value::integer))?;
            let path = file
                .get(b"path.utf-8")
                .or_else(|| file.get(b"path"))
                .and_then(Value::list)
                .ok_or(Error::Invalid("missing file path"))?;
            if path.is_empty() {
                return Err(Error::Invalid("empty file path"));
            }
            let mut path_bytes = name.to_vec();
            for item in path {
                let component = item.bytes().ok_or(Error::Invalid("invalid file path"))?;
                path_bytes = append_path(&path_bytes, component)?;
            }
            out.push(FileEntry {
                path: path_bytes,
                length,
            });
        }
        Ok(out)
    } else {
        let length = positive(info.get(b"length").and_then(Value::integer))?;
        Ok(vec![FileEntry {
            path: name.to_vec(),
            length,
        }])
    }
}

fn parse_v2(info: &Value<'_>) -> Result<Vec<FileEntry>, Error> {
    let tree = info
        .get(b"file tree")
        .ok_or(Error::Invalid("missing v2 file tree"))?;
    let mut files = Vec::new();
    visit_tree(tree, &[], &mut files, 0)?;
    Ok(files)
}

fn visit_tree(
    node: &Value<'_>,
    path: &[u8],
    result: &mut Vec<FileEntry>,
    depth: usize,
) -> Result<(), Error> {
    if depth > 40 {
        return Err(Error::Invalid("file tree too deep"));
    }
    let entries = node
        .dict()
        .ok_or(Error::Invalid("invalid file tree node"))?;
    if let Some((_, leaf)) = entries.iter().find(|(key, _)| key.is_empty()) {
        if entries.len() != 1 {
            return Err(Error::Invalid("ambiguous file tree leaf"));
        }
        let length = positive(leaf.get(b"length").and_then(Value::integer))?;
        if path.is_empty() {
            return Err(Error::Invalid("empty v2 file path"));
        }
        if result.len() >= MAX_FILES {
            return Err(Error::Invalid("too many files"));
        }
        result.push(FileEntry {
            path: path.to_vec(),
            length,
        });
        return Ok(());
    }
    for (key, child) in entries {
        let next = append_path(path, key)?;
        visit_tree(child, &next, result, depth + 1)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_v1_info_and_rejects_mismatch() {
        let bytes =
            b"d6:lengthi9e4:name8:test.mkv12:piece lengthi16384e6:pieces20:abcdefghijklmnopqrste";
        let hash: [u8; 20] = Sha1::digest(bytes).into();
        let record = decode_info(bytes, &hash).unwrap();
        assert_eq!(record.files[0].length, 9);
        assert_eq!(record.files[0].path, b"test.mkv");
        assert_eq!(record.v1, Some(hash));
        assert_eq!(record.v2, None);
        assert!(matches!(
            decode_info(bytes, &[0; 20]),
            Err(Error::HashMismatch)
        ));
    }
}
