//! Per-block adaptive token dictionary. All source bytes are reconstructed exactly.
//!
//! This is deliberately a reference implementation, not a promise that a
//! dictionary outperforms Zstd. encode_best compares actual output sizes.

use crate::{TorrentRecord, MAX_DICTIONARY_ENTRIES};
use std::collections::HashMap;

#[derive(Default)]
pub(crate) struct Dictionary {
    entries: Vec<Vec<u8>>,
    ids: HashMap<Vec<u8>, usize>,
}

impl Dictionary {
    pub(crate) fn build(records: &[TorrentRecord]) -> Self {
        let mut frequency: HashMap<Vec<u8>, u32> = HashMap::new();
        for record in records {
            count_tokens(&record.name, &mut frequency);
            for file in &record.files {
                count_tokens(&file.path, &mut frequency);
            }
        }

        // Estimate savings before accepting a candidate. Account for its
        // dictionary bytes, entry length, and the token reference overhead.
        let mut candidates: Vec<(Vec<u8>, u64)> = frequency
            .into_iter()
            .filter_map(|(token, occurrences)| {
                let len = token.len() as u64;
                let count = u64::from(occurrences);
                let gross = count.saturating_mul(len.saturating_sub(2));
                let overhead = len + 6;
                if count >= 3 && gross > overhead {
                    Some((token, gross - overhead))
                } else {
                    None
                }
            })
            .collect();
        candidates.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        let entries: Vec<Vec<u8>> = candidates
            .into_iter()
            .take(MAX_DICTIONARY_ENTRIES)
            .map(|(bytes, _)| bytes)
            .collect();
        Self::from_entries(entries)
    }

    pub(crate) fn from_entries(entries: Vec<Vec<u8>>) -> Self {
        let ids = entries
            .iter()
            .enumerate()
            .map(|(id, token)| (token.clone(), id))
            .collect();
        Self { entries, ids }
    }

    pub(crate) fn entries(&self) -> &[Vec<u8>] {
        &self.entries
    }

    pub(crate) fn get(&self, id: usize) -> Option<&[u8]> {
        self.entries.get(id).map(Vec::as_slice)
    }

    pub(crate) fn encode_path(&self, path: &[u8], encoded: &mut Vec<u8>) {
        let mut cursor = 0;
        let mut literal = Vec::new();
        scan_words(path, |start, end| {
            if let Some(&id) = self.ids.get(&path[start..end]) {
                literal.extend_from_slice(&path[cursor..start]);
                flush_literal(&mut literal, encoded);
                encoded.push(1);
                super::format::write_varint(id as u64, encoded);
                cursor = end;
            }
        });
        literal.extend_from_slice(&path[cursor..]);
        flush_literal(&mut literal, encoded);
    }
}

fn flush_literal(literal: &mut Vec<u8>, encoded: &mut Vec<u8>) {
    if !literal.is_empty() {
        encoded.push(0);
        super::format::write_varint(literal.len() as u64, encoded);
        encoded.extend_from_slice(literal);
        literal.clear();
    }
}

fn count_tokens(path: &[u8], frequency: &mut HashMap<Vec<u8>, u32>) {
    scan_words(path, |start, end| {
        let token = &path[start..end];
        if (4..=128).contains(&token.len()) {
            let counter = frequency.entry(token.to_vec()).or_default();
            *counter = counter.saturating_add(1);
        }
    });
}

/// ASCII letters/digits plus high bytes. High bytes preserve UTF-8 and
/// arbitrary non-UTF8 paths without any string normalization or data loss.
fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte >= 0x80
}

fn scan_words(path: &[u8], mut visit: impl FnMut(usize, usize)) {
    let mut pos = 0;
    while pos < path.len() {
        if !is_word(path[pos]) {
            pos += 1;
            continue;
        }
        let start = pos;
        while pos < path.len() && is_word(path[pos]) {
            pos += 1;
        }
        visit(start, pos);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_tokens_do_not_trigger_underflow() {
        let record = TorrentRecord {
            v1: Some([7; 20]),
            v2: None,
            name: b"NeverRepeatedWord".to_vec(),
            files: Vec::new(),
        };
        assert!(Dictionary::build(&[record]).entries().is_empty());
    }
}
