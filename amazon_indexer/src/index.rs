/// Reads an Amazon Reviews 2023 JSONL file (one review per line) and indexes it into tantivy.
///
/// Usage:
///   cargo run --bin index -- <jsonl_file> <index_dir>
///
/// Example:
///   cargo run --release --bin index -- data/Video_Games.jsonl index/

mod schema;

use schema::AmazonSchema;

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::Instant;

use serde::Deserialize;
use tantivy::{doc, Index, IndexWriter};

// ── JSON record ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Review {
    rating:           f64,
    #[serde(default)]
    title:            String,
    #[serde(default)]
    text:             String,
    asin:             String,
    user_id:          String,
    timestamp:        u64,   // milliseconds since epoch
    #[serde(default)]
    helpful_vote:     u64,
    #[serde(default)]
    verified_purchase: bool,
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() -> tantivy::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("Usage: index <jsonl_file> <index_dir>");
        std::process::exit(1);
    }
    let jsonl_path = &args[1];
    let index_dir  = &args[2];

    // ── Schema & index ───────────────────────────────────────────────────────
    let s = AmazonSchema::build();

    std::fs::create_dir_all(index_dir)?;
    let index = Index::create_in_dir(Path::new(index_dir), s.schema.clone())?;

    // 200 MB write buffer — good throughput without excessive RAM
    let mut writer: IndexWriter = index.writer(200_000_000)?;

    // ── Indexing loop ────────────────────────────────────────────────────────
    let file    = File::open(jsonl_path)?;
    let reader  = BufReader::with_capacity(1 << 20, file); // 1 MB read buffer
    let start   = Instant::now();
    let mut count   = 0u64;
    let mut skipped = 0u64;

    for (line_no, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() { continue; }

        let review: Review = match serde_json::from_str(&line) {
            Ok(r)  => r,
            Err(e) => {
                eprintln!("line {}: parse error: {e}", line_no + 1);
                skipped += 1;
                continue;
            }
        };

        writer.add_document(doc!(
            s.rating           => review.rating,
            s.title            => review.title,
            s.body             => review.text,
            s.asin             => review.asin,
            s.user_id          => review.user_id,
            s.timestamp_ms     => review.timestamp,
            s.helpful_vote     => review.helpful_vote,
            s.verified_purchase => review.verified_purchase,
        ))?;

        count += 1;

        // Commit every 1 million documents to keep memory bounded
        if count % 1_000_000 == 0 {
            writer.commit()?;
            let elapsed = start.elapsed().as_secs_f64();
            eprintln!(
                "[{count:>10} docs | {:.1}s | {:.0} docs/s]",
                elapsed,
                count as f64 / elapsed
            );
        }
    }

    // Final commit
    writer.commit()?;

    let elapsed = start.elapsed().as_secs_f64();
    eprintln!("Done: {count} docs indexed, {skipped} skipped, {elapsed:.1}s ({:.0} docs/s)",
        count as f64 / elapsed);

    Ok(())
}
