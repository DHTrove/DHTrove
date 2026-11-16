//! Deterministic synthetic workload: comparable codecs, identical records.
//! Real-corpus benchmarking will be added before any performance claims.

use dhtrove_storage::{decode_block, encode_block, Codec, FileEntry, TorrentRecord, MAX_RECORDS};
use std::error::Error;
use std::time::Instant;

fn main() -> Result<(), Box<dyn Error>> {
    let count = match std::env::args().nth(1) {
        Some(value) => value.parse::<usize>()?,
        None => 2_000,
    };
    if count > MAX_RECORDS {
        return Err(format!("count must be at most {MAX_RECORDS}").into());
    }

    let records = fixture(count);
    let file_count: usize = records.iter().map(|r| r.files.len()).sum();
    println!("# DHTrove storage codec benchmark\n");
    println!("- Corpus: deterministic synthetic filenames (not real DHT data)");
    println!("- Torrent records: {count}");
    println!("- File entries: {file_count}");
    println!("- Size includes format header, infohashes and dictionary overhead");
    println!("- Times represent a single local run (not statistically significant)\n");
    println!("| Codec | Bytes | Ratio to raw | Encode ms | Decode ms |");
    println!("|---|---:|---:|---:|---:|");

    let mut raw_size = 0usize;
    let mut smallest = (Codec::Raw, usize::MAX);
    for codec in Codec::ALL {
        let start = Instant::now();
        let block = encode_block(&records, codec)?;
        let encode_ms = start.elapsed().as_secs_f64() * 1_000.0;
        let start = Instant::now();
        let decoded = decode_block(&block)?;
        let decode_ms = start.elapsed().as_secs_f64() * 1_000.0;
        if decoded != records {
            return Err(format!("round-trip mismatch using {}", codec.name()).into());
        }
        if codec == Codec::Raw {
            raw_size = block.len();
        }
        if block.len() < smallest.1 {
            smallest = (codec, block.len());
        }
        println!(
            "| {} | {} | {:.3}x | {:.2} | {:.2} |",
            codec.name(),
            block.len(),
            block.len() as f64 / raw_size as f64,
            encode_ms,
            decode_ms
        );
    }
    println!(
        "\nBest size: **{}** ({} bytes; {:.2}% smaller than raw encoding).",
        smallest.0.name(),
        smallest.1,
        (1.0 - smallest.1 as f64 / raw_size as f64) * 100.0
    );
    Ok(())
}

fn fixture(count: usize) -> Vec<TorrentRecord> {
    let mut records = Vec::with_capacity(count);
    for i in 0..count {
        let season = i % 15 + 1;
        let source = match i % 3 {
            0 => "WEB-DL",
            1 => "BluRay",
            _ => "WEBRip",
        };
        let mut v1 = [0u8; 20];
        v1[..8].copy_from_slice(&(i as u64).to_le_bytes());
        v1[8..16].copy_from_slice(&((i as u64).wrapping_mul(0x9e3779b97f4a7c15)).to_be_bytes());
        let mut v2 = [0u8; 32];
        v2[..8].copy_from_slice(&(i as u64).to_be_bytes());
        v2[8..16].copy_from_slice(&((i as u64).wrapping_mul(0xd6e8feb86659fd93)).to_le_bytes());

        let name = format!("Sample Show Season {season:02} 1080p {source} Release-{i:06}");
        let files = (0..8)
            .map(|episode| FileEntry {
                path: format!(
                    "Sample Show/Season {season:02}/Sample.Show.S{season:02}E{episode:02}.1080p.{source}.mkv"
                )
                .into_bytes(),
                length: 380_000_000 + (i as u64) * 17 + episode * 1000,
            })
            .collect();

        records.push(TorrentRecord {
            v1: Some(v1),
            v2: (i % 11 == 0).then_some(v2),
            name: name.into_bytes(),
            files,
        });
    }
    records
}
