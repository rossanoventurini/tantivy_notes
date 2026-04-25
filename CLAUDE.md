# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
# Build
cargo build --all-features

# Test (recommended)
cargo test --tests --lib
make test   # equivalent

# Single test
cargo test test_name -- --nocapture

# Specific module
cargo test --lib core::tests

# With features
cargo test --features "mmap,stemmer" --lib

# Long-running tests (ignored by default)
cargo test indexing_unsorted -- --ignored
cargo test indexing_sorted -- --ignored

# Format (requires nightly)
cargo +nightly fmt --all
make fmt   # equivalent

# Lint
cargo clippy --tests

# Doctests
cargo test --doc --features "mmap,stemmer,lz4-compression" --verbose --workspace
```

## Architecture

Tantivy is a full-text search engine library inspired by Lucene. The index is composed of immutable **segments** (each identified by a UUID). Writes are batched; on commit, one segment per indexing thread is flushed to disk and `meta.json` is updated atomically. Segment merges happen in background threads.

### Workspace Crates

| Crate | Purpose |
|-------|---------|
| `tantivy` (root) | Core library: indexing, searching, queries, collectors |
| `query-grammar` | Parser combinator that converts user query strings into AST |
| `tokenizer-api` | Stable public API for tokenizer implementations |
| `columnar` | Column-oriented storage for fast field values |
| `sstable` | Sorted string table format used by the term dictionary |
| `stacker` | In-memory document accumulator during indexing |
| `bitpacker` | SIMD (SSE2) integer compression |
| `common` | Shared I/O and serialization utilities |
| `ownedbytes` | Owned byte arrays for mmap-backed data |

### Core Modules (`src/`)

| Module | Responsibility |
|--------|---------------|
| `core/` | High-level index lifecycle, `Searcher` (snapshot over segments), `SegmentReader` |
| `directory/` | `Directory` trait abstraction; `MmapDirectory` and `RamDirectory` implementations |
| `schema/` | Field type definitions, `Document`, `Schema` builder |
| `indexer/` | `IndexWriter`, document batching, segment flush |
| `store/` | Row-oriented compressed docstore (LZ4/Zstd); use sparingly—not for per-doc access |
| `fastfield/` | Column-oriented O(1) random access per `DocId` (bitpacked); Lucene's DocValues equivalent |
| `termdict/` | Term dictionary: `Term → TermOrdinal` via FST, then `TermOrdinal → TermInfo` |
| `postings/` | Posting lists: delta-encoded, bitpacked blocks of 128 `DocId`s + term frequencies |
| `positions/` | Per-term token positions, required for phrase queries |
| `fieldnorm/` | One-byte-per-doc field length, used in BM25 scoring |
| `tokenizer/` | Text processing pipeline: tokenizers + filter chain |
| `query/` | `Query`, `Weight`, `Scorer` traits; `BooleanQuery`, `TermQuery`, phrase queries, etc. |
| `collector/` | `Collector` trait; `TopDocs`, facets, aggregations |
| `aggregation/` | Histogram, range, metric aggregations |
| `snippet/` | Search result snippet/highlight generation |

### Inverted Index Data Flow

```
Term → FST (termdict) → TermOrdinal → TermInfo → posting list offset → DocId iterator
```

Each data structure follows the same pattern: **writer** (in-memory mutable) → **serializer** (flush to disk) → **reader** (mmapped read-only).

### Key Traits to Know

- `Query` / `Weight` / `Scorer` — implement to add new query types
- `Collector` — implement to define custom result aggregation
- `Directory` — implement to add custom storage backends
- `Tokenizer` / `TokenFilter` — implement for custom text processing

## Features

Default: `mmap`, `stopwords`, `lz4-compression`, `columnar-zstd-compression`, `stemmer`

Notable optional features:
- `failpoints` — enables fail injection for testing (used in `tests/failpoints/`)
- `quickwit` — distributed search extensions
- `zstd-compression` — alternative docstore compression

## Minimum Rust Version

1.86 (stable). Nightly required only for `cargo fmt`.

## Notes

- `DocId` is segment-local and compact `[0, max_doc)`. Deletes use a tombstone bitset (`.del` files), not in-place removal.
- The docstore should not be hit more than ~100 times per query; use fast fields for per-doc value access during scoring/collection.
- Failpoint tests run in a separate binary (`tests/failpoints/`) to avoid global state conflicts.
- Benchmarks require nightly: `cargo +nightly bench --no-run`.
