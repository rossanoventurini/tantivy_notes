# Tantivy Study Plan

## Intent

We want to deeply understand tantivy at two distinct levels:

1. **User level** — Build real search applications: design schemas, index documents, write
   queries, collect and aggregate results, understand the operational model (commits,
   snapshots, deletes, reload policies).

2. **Implementation level** — Understand the internal machinery: how segments are structured
   on disk, how the inverted index is built and read, how postings are compressed, how BM25
   is actually computed, how merges work, and how all the data structures fit together.
   The goal is to be able to read, reason about, and contribute to the tantivy codebase.

---

## Files

| File | Purpose |
|------|---------|
| `user_tutorial.md` | Phase 1: user-level concepts with Q&A and exercises after each section |
| `impl_tutorial.md` | Phase 2: implementation internals with precise source references |
| `datasets.md` | Public datasets for practice |
| `answers_phase1.md` | Your written answers to the Phase 1 review questions (create this yourself) |
| `impl_notes/session_N.md` | Per-session notes during Phase 2 source reading |

---

## Phase 1 — User Level

| Session | Topic | Read | Exercises |
|---------|-------|------|-----------|
| 1 | Schema design: TEXT/STRING/STORED/FAST, field types | `user_tutorial.md` §3, `examples/basic_search.rs` | Exercise A, Exercise B (partial) |
| 2 | IndexWriter, commit lifecycle, delete, update | `user_tutorial.md` §4–5 | Exercise B (full) |
| 3 | Queries: QueryParser, TermQuery, BooleanQuery, RangeQuery, FuzzyQuery, PhraseQuery | `user_tutorial.md` §6.1–6.3, `examples/fuzzy_search.rs`, `examples/integer_range_search.rs` | Exercise C |
| 4 | Collectors, BM25 scoring, score explanation | `user_tutorial.md` §6.4–6.5, `examples/basic_search.rs` | Exercise D |
| 5 | Docstore, snippets | `user_tutorial.md` §7–8, `examples/snippet.rs` | Exercise E |
| 6 | Facets | `user_tutorial.md` §9, `examples/faceted_search.rs` | Exercise F |
| 7 | Aggregations | `user_tutorial.md` §10, `examples/aggregation.rs` | Exercise G |
| 8 | **Review** — answer all 12 questions in `user_tutorial.md` §13 without notes | — | Write `answers_phase1.md` |

---

## Phase 2 — Implementation Level

| Session | Topic | Tutorial section | Key source files |
|---------|-------|-----------------|-----------------|
| 9 | Segment anatomy: files, extensions, meta.json, DocId | `impl_tutorial.md` §1–2 | `src/index/segment_component.rs`, `src/index/index_meta.rs` |
| 10 | Inverted index: term encoding, FST, SSTable, TermInfo | `impl_tutorial.md` §3 | `src/termdict/`, `src/schema/term.rs`, `sstable/` |
| 11 | Posting lists: delta coding, bitpacking, skip lists, vint | `impl_tutorial.md` §4 | `src/postings/compression/`, `src/postings/skip.rs`, `bitpacker/` |
| 12 | Positions and phrase queries | `impl_tutorial.md` §5 | `src/positions/`, `src/query/phrase_query/` |
| 13 | Fast fields: column layout, bitpacking, alive bitset | `impl_tutorial.md` §6 | `src/fastfield/`, `columnar/` |
| 14 | Field norms: 256-entry table, BM25 cache trick | `impl_tutorial.md` §7 | `src/fieldnorm/code.rs`, `src/query/bm25.rs` |
| 15 | Docstore: 16 KB blocks, LZ4/Zstd, skip index, 100-block cache | `impl_tutorial.md` §8 | `src/store/` |
| 16 | BM25 scoring: full formula, IDF, TF normalisation, multi-segment stats | `impl_tutorial.md` §9 | `src/query/bm25.rs` |
| 17 | Indexing pipeline: Stacker → sort terms → serialise all files | `impl_tutorial.md` §10 | `src/indexer/segment_writer.rs`, `src/postings/serializer.rs`, `stacker/` |
| 18 | Segment merges: LogMergePolicy layers, background execution | `impl_tutorial.md` §11 | `src/indexer/log_merge_policy.rs`, `src/indexer/merger.rs` |
| 19 | Range queries: inverted index scan vs fast field scan | `impl_tutorial.md` §12 | `src/query/range_query/` |
| 20 | Query execution: Query → Weight → Scorer chain | `impl_tutorial.md` §13 | `src/query/query.rs`, `weight.rs`, `scorer.rs` |
| 21 | **Capstone** — trace a full query from string to scored results | `impl_tutorial.md` §14 | All of the above |

---

## Rules for Each Session

1. **Read the tutorial section first**, then open the cited source files.
2. **Answer the section's questions in writing** before moving on.
3. **Complete at least one exercise** per session. Write your solutions in `impl_notes/`.
4. **Never skip a session.** The sessions are ordered by dependency — later ones assume
   earlier ones.

---

## Current Status

- [x] `user_tutorial.md` — complete with Q&A and exercises after every section
- [x] `impl_tutorial.md` — complete (14 sections, source references throughout)
- [x] `datasets.md` — complete
- [x] `amazon_indexer` — compiles (`cargo build --release` passes)
- [ ] Video_Games.jsonl — downloading (PID 1225637)
- [ ] Sessions 1–8 (Phase 1) — not started
- [ ] Sessions 9–21 (Phase 2) — not started
