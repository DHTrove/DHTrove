use crate::dictionary::Dictionary;
use crate::{
    Codec, Error, FileEntry, TorrentRecord, MAX_BLOCK_BYTES, MAX_DICTIONARY_ENTRIES, MAX_FILES,
    MAX_PATH_BYTES, MAX_RECORDS,
};

const MAGIC: &[u8; 4] = b"DHTB";
const VERSION: u8 = 1;

/// Encode multiple records in a single block. Its dictionary spans all records.
pub fn encode_block(records: &[TorrentRecord], codec: Codec) -> Result<Vec<u8>, Error> {
    validate(records)?;

    let base = codec.base();
    let dictionary = if base == Codec::Dictionary {
        Dictionary::build(records)
    } else {
        Dictionary::default()
    };

    let mut raw = Vec::new();
    if base == Codec::Dictionary {
        write_varint(dictionary.entries().len() as u64, &mut raw);
        for entry in dictionary.entries() {
            write_varint(entry.len() as u64, &mut raw);
            raw.extend_from_slice(entry);
        }
    }

    write_varint(records.len() as u64, &mut raw);
    let mut previous = Vec::new();
    for record in records {
        let flags = u8::from(record.v1.is_some()) | (u8::from(record.v2.is_some()) << 1);
        raw.push(flags);
        if let Some(v1) = record.v1 {
            raw.extend_from_slice(&v1);
        }
        if let Some(v2) = record.v2 {
            raw.extend_from_slice(&v2);
        }

        write_path(&record.name, base, &dictionary, &mut previous, &mut raw);
        write_varint(record.files.len() as u64, &mut raw);
        for file in &record.files {
            write_path(&file.path, base, &dictionary, &mut previous, &mut raw);
            write_varint(file.length, &mut raw);
        }
        if raw.len() > MAX_BLOCK_BYTES {
            return Err(Error::Limit("uncompressed block is too large"));
        }
    }

    let payload = if codec.compressed() {
        zstd::bulk::compress(&raw, 3)?
    } else {
        raw.clone()
    };

    let mut result = Vec::with_capacity(payload.len() + 16);
    result.extend_from_slice(MAGIC);
    result.push(VERSION);
    result.push(codec as u8);
    write_varint(raw.len() as u64, &mut result);
    // A block checksum detects corruption independently of compression.
    result.extend_from_slice(&crc32fast::hash(&raw).to_le_bytes());
    result.extend_from_slice(&payload);
    if result.len() > MAX_BLOCK_BYTES {
        return Err(Error::Limit("encoded block is too large"));
    }
    Ok(result)
}

/// Try each lossless encoding and return the smallest *whole* block.
/// This explicitly avoids expansion for small or irregular inputs.
pub fn encode_best(records: &[TorrentRecord]) -> Result<(Codec, Vec<u8>), Error> {
    let mut best: Option<(Codec, Vec<u8>)> = None;
    for codec in Codec::ALL {
        let candidate = encode_block(records, codec)?;
        if best
            .as_ref()
            .is_none_or(|(_, bytes)| candidate.len() < bytes.len())
        {
            best = Some((codec, candidate));
        }
    }
    best.ok_or(Error::Invalid("no available codec"))
}

/// Decode a versioned block and verify length, checksum and format limits.
pub fn decode_block(input: &[u8]) -> Result<Vec<TorrentRecord>, Error> {
    if input.len() > MAX_BLOCK_BYTES {
        return Err(Error::Limit("encoded block is too large"));
    }
    let mut header = Reader::new(input);
    if header.take(4)? != MAGIC {
        return Err(Error::Invalid("incorrect magic"));
    }
    if header.byte()? != VERSION {
        return Err(Error::Invalid("unsupported format version"));
    }
    let codec = Codec::try_from(header.byte()?)?;
    let expected_len = header.length(MAX_BLOCK_BYTES)?;
    let checksum_bytes = header.take(4)?;
    let expected_crc = u32::from_le_bytes(
        checksum_bytes
            .try_into()
            .map_err(|_| Error::Invalid("invalid checksum"))?,
    );
    let payload = header.remaining();

    let raw = if codec.compressed() {
        zstd::bulk::decompress(payload, expected_len)?
    } else {
        if payload.len() != expected_len {
            return Err(Error::Invalid("wrong payload length"));
        }
        payload.to_vec()
    };
    if raw.len() != expected_len || crc32fast::hash(&raw) != expected_crc {
        return Err(Error::Invalid("payload length or checksum mismatch"));
    }

    let mut input = Reader::new(&raw);
    let base = codec.base();
    let dictionary = if base == Codec::Dictionary {
        let count = input.length(MAX_DICTIONARY_ENTRIES)?;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let len = input.length(128)?;
            if len < 4 {
                return Err(Error::Invalid("dictionary token too short"));
            }
            entries.push(input.take(len)?.to_vec());
        }
        Dictionary::from_entries(entries)
    } else {
        Dictionary::default()
    };

    let record_count = input.length(MAX_RECORDS)?;
    let mut records = Vec::with_capacity(record_count);
    let mut previous = Vec::new();
    let mut file_count_total = 0usize;
    for _ in 0..record_count {
        let flags = input.byte()?;
        if flags == 0 || flags & !3 != 0 {
            return Err(Error::Invalid("invalid infohash flags"));
        }
        let v1 = if flags & 1 != 0 {
            Some(
                input
                    .take(20)?
                    .try_into()
                    .map_err(|_| Error::Invalid("invalid v1 infohash"))?,
            )
        } else {
            None
        };
        let v2 = if flags & 2 != 0 {
            Some(
                input
                    .take(32)?
                    .try_into()
                    .map_err(|_| Error::Invalid("invalid v2 infohash"))?,
            )
        } else {
            None
        };
        let name = read_path(&mut input, base, &dictionary, &mut previous)?;
        let file_count = input.length(MAX_FILES)?;
        file_count_total = file_count_total
            .checked_add(file_count)
            .ok_or(Error::Limit("file count overflow"))?;
        if file_count_total > MAX_FILES {
            return Err(Error::Limit("too many files"));
        }
        let mut files = Vec::with_capacity(file_count);
        for _ in 0..file_count {
            let path = read_path(&mut input, base, &dictionary, &mut previous)?;
            let length = input.varint()?;
            files.push(FileEntry { path, length });
        }
        records.push(TorrentRecord {
            v1,
            v2,
            name,
            files,
        });
    }
    if !input.remaining().is_empty() {
        return Err(Error::Invalid("trailing payload bytes"));
    }
    Ok(records)
}

fn validate(records: &[TorrentRecord]) -> Result<(), Error> {
    if records.len() > MAX_RECORDS {
        return Err(Error::Limit("too many torrents in block"));
    }
    let mut files = 0usize;
    for record in records {
        if record.v1.is_none() && record.v2.is_none() {
            return Err(Error::Invalid("torrent needs a v1 or v2 infohash"));
        }
        check_path(&record.name)?;
        files = files
            .checked_add(record.files.len())
            .ok_or(Error::Limit("file count overflow"))?;
        if files > MAX_FILES {
            return Err(Error::Limit("too many files in block"));
        }
        for file in &record.files {
            check_path(&file.path)?;
        }
    }
    Ok(())
}

fn check_path(path: &[u8]) -> Result<(), Error> {
    if path.len() > MAX_PATH_BYTES {
        return Err(Error::Limit("path too long"));
    }
    Ok(())
}

fn write_path(
    path: &[u8],
    codec: Codec,
    dictionary: &Dictionary,
    previous: &mut Vec<u8>,
    output: &mut Vec<u8>,
) {
    match codec {
        Codec::Raw => {
            write_varint(path.len() as u64, output);
            output.extend_from_slice(path);
        }
        Codec::Prefix => {
            let common = path
                .iter()
                .zip(previous.iter())
                .take_while(|(left, right)| left == right)
                .count();
            write_varint(common as u64, output);
            write_varint((path.len() - common) as u64, output);
            output.extend_from_slice(&path[common..]);
            previous.clear();
            previous.extend_from_slice(path);
        }
        Codec::Dictionary => {
            let mut token_stream = Vec::new();
            dictionary.encode_path(path, &mut token_stream);
            write_varint(token_stream.len() as u64, output);
            output.extend_from_slice(&token_stream);
        }
        _ => unreachable!("write_path receives only a base codec"),
    }
}

fn read_path(
    input: &mut Reader<'_>,
    codec: Codec,
    dictionary: &Dictionary,
    previous: &mut Vec<u8>,
) -> Result<Vec<u8>, Error> {
    match codec {
        Codec::Raw => {
            let len = input.length(MAX_PATH_BYTES)?;
            Ok(input.take(len)?.to_vec())
        }
        Codec::Prefix => {
            let prefix = input.length(MAX_PATH_BYTES)?;
            if prefix > previous.len() {
                return Err(Error::Invalid("prefix exceeds previous path"));
            }
            let suffix = input.length(MAX_PATH_BYTES - prefix)?;
            let mut path = previous[..prefix].to_vec();
            path.extend_from_slice(input.take(suffix)?);
            *previous = path.clone();
            Ok(path)
        }
        Codec::Dictionary => {
            let len = input.length(MAX_PATH_BYTES * 3 + 64)?;
            let encoded = input.take(len)?;
            let mut reader = Reader::new(encoded);
            let mut path = Vec::new();
            while !reader.remaining().is_empty() {
                match reader.byte()? {
                    0 => {
                        let size = reader.length(MAX_PATH_BYTES - path.len())?;
                        path.extend_from_slice(reader.take(size)?);
                    }
                    1 => {
                        let id = reader.length(MAX_DICTIONARY_ENTRIES)?;
                        let token = dictionary
                            .get(id)
                            .ok_or(Error::Invalid("unknown dictionary token"))?;
                        if token.len() > MAX_PATH_BYTES - path.len() {
                            return Err(Error::Limit("decoded path too long"));
                        }
                        path.extend_from_slice(token);
                    }
                    _ => return Err(Error::Invalid("unknown token marker")),
                }
            }
            Ok(path)
        }
        _ => Err(Error::Invalid("unsupported base codec")),
    }
}

pub(crate) fn write_varint(mut value: u64, output: &mut Vec<u8>) {
    while value >= 0x80 {
        output.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.pos..]
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(Error::Limit("offset overflow"))?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(Error::Invalid("truncated block"))?;
        self.pos = end;
        Ok(slice)
    }

    fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    fn varint(&mut self) -> Result<u64, Error> {
        let mut result = 0u64;
        for step in 0..10 {
            let byte = self.byte()?;
            if step == 9 && byte > 1 {
                return Err(Error::Invalid("varint overflow"));
            }
            result |= u64::from(byte & 0x7f) << (step * 7);
            if byte & 0x80 == 0 {
                return Ok(result);
            }
        }
        Err(Error::Invalid("varint overflow"))
    }

    fn length(&mut self, max: usize) -> Result<usize, Error> {
        let n = self.varint()?;
        let len = usize::try_from(n).map_err(|_| Error::Limit("integer overflow"))?;
        if len > max {
            return Err(Error::Limit("length exceeds maximum"));
        }
        Ok(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<TorrentRecord> {
        let mut result = Vec::new();
        for i in 0..24_u32 {
            let mut hash1 = [0u8; 20];
            hash1[..4].copy_from_slice(&i.to_le_bytes());
            let mut hash2 = [0u8; 32];
            hash2[..4].copy_from_slice(&i.to_be_bytes());
            result.push(TorrentRecord {
                v1: (i % 3 != 1).then_some(hash1),
                v2: (i % 3 != 0).then_some(hash2),
                name: format!("Season {i:02} 高清 WEB-DL").into_bytes(),
                files: vec![
                    FileEntry {
                        path: format!("Series/Season {i:02}/Episode 001.mkv").into_bytes(),
                        length: 4_500_000_000 + u64::from(i),
                    },
                    FileEntry {
                        path: format!("Series/Season {i:02}/Episode 002.mkv").into_bytes(),
                        length: 0,
                    },
                    FileEntry {
                        path: vec![0xff, b'/', b'a', 0, b'/', b'b'],
                        length: u64::MAX,
                    },
                ],
            });
        }
        result
    }

    #[test]
    fn all_codecs_roundtrip_exact_bytes_and_order() {
        let data = fixture();
        for codec in Codec::ALL {
            let encoded = encode_block(&data, codec).unwrap();
            let decoded = decode_block(&encoded).unwrap();
            assert_eq!(decoded, data, "codec {}", codec.name());
        }
    }

    #[test]
    fn adaptive_never_exceeds_raw_encoding() {
        let data = fixture();
        let raw = encode_block(&data, Codec::Raw).unwrap();
        let (selected, compact) = encode_best(&data).unwrap();
        assert!(compact.len() <= raw.len(), "selected {}", selected.name());
        assert_eq!(decode_block(&compact).unwrap(), data);
    }

    #[test]
    fn empty_block_and_empty_paths_roundtrip() {
        for codec in Codec::ALL {
            assert!(decode_block(&encode_block(&[], codec).unwrap())
                .unwrap()
                .is_empty());
        }
        let rows = vec![TorrentRecord {
            v1: Some([7; 20]),
            v2: None,
            name: Vec::new(),
            files: vec![FileEntry {
                path: Vec::new(),
                length: 0,
            }],
        }];
        for codec in Codec::ALL {
            assert_eq!(
                decode_block(&encode_block(&rows, codec).unwrap()).unwrap(),
                rows
            );
        }
    }

    #[test]
    fn corruption_and_truncation_are_detected() {
        let mut data = encode_block(&fixture(), Codec::Raw).unwrap();
        data[0] ^= 0x80;
        assert!(decode_block(&data).is_err());
        let mut data = encode_block(&fixture(), Codec::Raw).unwrap();
        let last = data.len() - 1;
        data[last] ^= 0x80;
        assert!(decode_block(&data).is_err());
        data.pop();
        assert!(decode_block(&data).is_err());
        let mut data = encode_block(&fixture(), Codec::RawZstd).unwrap();
        data.truncate(data.len() - 1);
        assert!(decode_block(&data).is_err());
    }

    #[test]
    fn rejects_invalid_infohash_and_oversized_paths() {
        let invalid = vec![TorrentRecord {
            v1: None,
            v2: None,
            name: b"missing hash".to_vec(),
            files: Vec::new(),
        }];
        assert!(encode_best(&invalid).is_err());

        let huge = vec![TorrentRecord {
            v1: Some([0; 20]),
            v2: None,
            name: vec![b'x'; MAX_PATH_BYTES + 1],
            files: Vec::new(),
        }];
        assert!(encode_block(&huge, Codec::Raw).is_err());
    }
}
