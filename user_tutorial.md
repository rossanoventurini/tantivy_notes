# Tantivy User Tutorial

A ground-up guide to using tantivy as a search library. Each concept is followed immediately
by comprehension questions and a coding exercise. Work through them before reading the next
section — that is where the learning happens.

---

## 1. What is tantivy?

Tantivy is a full-text search **library** written in Rust. You embed it in your application;
it is not a standalone server like Elasticsearch or Solr. Its design and vocabulary are
strongly inspired by Apache Lucene.

Its core promise: given a large collection of documents and a query, return the most relevant
documents very quickly. Relevance is scored with **BM25** — the same algorithm Lucene and
Elasticsearch use by default.

Beyond full-text search, tantivy also supports:

- Numeric, date, boolean and IP fields with **range queries**
- **Faceted navigation** — hierarchical categories like `/Electronics/Laptops`
- **Aggregations** — histograms, averages, min/max per group (Elasticsearch-compatible API)
- **Snippet and highlight** generation
- **Fuzzy** (approximate) matching with configurable edit distance
- **Phrase queries** — exact ordered sequences of terms
- **Custom scoring** — combine BM25 with fast-field values

### Questions

1. What is the fundamental difference between tantivy and Elasticsearch?
2. You want to build a search endpoint for a web app. Does tantivy give you an HTTP server?
   What does it give you?
3. Name three query types that tantivy supports beyond basic keyword search.

### Answers

1. Tantivy is a **library**; Elasticsearch is a **server**. You link tantivy into your
   Rust process and call its API directly — no daemon, no HTTP layer, no JVM. ES wraps
   Lucene in a distributed service with replication, sharding, REST/JSON, and a
   cluster manager. Tantivy gives you the search engine; you build everything else
   around it (or use a project like Quickwit that does so on top of tantivy).
2. No HTTP server. Tantivy gives you in-process Rust APIs: `Schema`, `Index`,
   `IndexWriter`, `IndexReader`/`Searcher`, query and collector traits. You bring your
   own transport (axum, actix, tonic, etc.) and call into the library from your
   handlers.
3. Three of: phrase queries (`PhraseQuery`), fuzzy queries (`FuzzyTermQuery`), range
   queries on numeric/date/bool fields (`RangeQuery`), Boolean queries
   (`BooleanQuery`), regex queries (`RegexQuery`), facets (`FacetCollector`),
   aggregations (`terms`, `histogram`, `range`, metric aggregations like `avg`/`sum`),
   "more like this" (`MoreLikeThisQuery`), disjunction-max (`DisjunctionMaxQuery`).

---

## 2. The Mental Model

Before writing any code, internalise this pipeline:

```
Schema ──► Index ──► IndexWriter ──► add_document() ──► commit()
                                                              │
                                                              ▼
                                              Reader ──► Searcher ──► search(Query, Collector)
                                                                             │
                                                                             ▼
                                                                       Vec<(Score, DocAddress)>
                                                                             │
                                                                             ▼
                                                                    searcher.doc(address)
```

| Object | Role |
|--------|------|
| `Schema` | Blueprint — declares every field, its type and indexing options. Fixed at index creation time. |
| `Index` | The actual index on disk or in RAM. Holds the schema and the segment data. |
| `IndexWriter` | The single gateway for all writes: add, delete, commit. |
| `IndexReader` | A long-lived pool of `Searcher` objects that reloads when the index changes. |
| `Searcher` | A frozen snapshot of the index. Acquire one per request, release when done. |
| `Query` | Defines which documents to match and how to score them. |
| `Collector` | Defines what to do with matched documents: top-k by score, count, facets, aggregations… |
| `DocAddress` | A segment-local pointer to one document. Use it to fetch from the docstore. |

The most important invariant: **documents are not searchable until after `commit()`**. A
commit is atomic — either all documents in the batch are visible, or none are.

### 2.1 The IndexWriter in depth

`IndexWriter` is the **sole write path** to the index. Its contract:

- **Exclusive**: only one `IndexWriter` can exist per index at a time, even across processes.
  Tantivy enforces this with a file lock (`meta.lock`). Attempting a second writer fails
  immediately with a `LockBusy` error.
- **Buffered**: documents added with `add_document()` are held in an in-memory write buffer
  (whose size you control). They are **not written to disk and not searchable** until you
  call `commit()`.
- **Multithreaded internally**: the writer spawns one indexing thread per logical CPU
  (configurable via `writer_with_num_threads`). Each thread maintains its own buffer and
  flushes exactly one segment on commit. With 4 threads and one commit you get 4 new segments.
- **Commit is atomic**: `commit()` flushes all buffered documents to disk and atomically
  updates `meta.json`. Readers see *all* committed documents or *none* of them — partial
  commits do not exist.
- **Rollback**: calling `rollback()` discards everything buffered since the last commit,
  as if those `add_document` calls never happened. Nothing on disk is touched.
- **Delete by term**: `delete_term(Term)` marks all documents that contain that term for
  deletion. The deletion is also buffered and becomes effective only after `commit()`.
  Tantivy does *not* remove the document from disk immediately; it writes a tombstone bitset
  (`.del` file). The document disappears from search results at the next reload, and is
  physically removed when the segment is later merged.

```rust
let mut writer: IndexWriter = index.writer(50_000_000)?;  // 50 MB buffer
writer.add_document(doc!(title => "Moby Dick"))?;
// doc is NOT searchable yet
writer.commit()?;
// NOW it is searchable (after the reader reloads — see below)
```

Cost of `commit()`: it flushes and fsync's segment files. This is a disk write, so it is
**expensive** relative to `add_document`. Batch large numbers of documents between commits
for throughput; do not commit after every document.

### 2.2 The IndexReader and Searcher in depth

`IndexReader` is a **long-lived object** you create once and reuse for the lifetime of the
application. Its job is to keep a pool of `Searcher` snapshots up to date.

**What happens when you call `reader.searcher()`?**

1. The reader returns a `Searcher` that represents a **frozen snapshot** of the index at the
   moment of the most recent reload. It wraps a set of `Arc`-reference-counted segment readers.
2. The `Searcher` itself is a lightweight struct — acquiring one is essentially just
   incrementing a few reference counts and is **extremely cheap** (no disk I/O, no locking
   beyond an atomic read).

**What happens when the Searcher is dropped?**

The `Searcher` holds `Arc` references to each segment reader it covers. Dropping it
decrements those reference counts. When the count reaches zero (i.e., no other searcher is
using that segment version), the underlying memory-mapped files are unmapped and the segment
files may be deleted if a merge has superseded them. This is also cheap — just reference
count decrements and possibly a few `munmap` calls. There is no cost to creating or
discarding searchers frequently.

**Should you reuse a Searcher across multiple queries?** No — and you shouldn't need to.
The right pattern is: one `Searcher` per request (or per query), acquired from the reader and
dropped when the response is sent. Holding a searcher longer than necessary delays the cleanup
of superseded segments.

**When does the reader see new commits?** Not automatically by default. You control this:

```rust
// Option A — explicit reload (e.g., call this after every commit)
reader.reload()?;

// Option B — create the reader with a background polling policy
let reader = index
    .reader_builder()
    .reload_policy(ReloadPolicy::OnCommitWithDelay)
    .try_into()?;
```

`OnCommitWithDelay` polls for new commits in a background thread (approximately every
500 ms). Until the reader reloads, new commits are invisible to all searchers — even if the
commit has been durably written to disk.

**Does a reload affect searchers that are already alive?** No. A reload reads `meta.json`,
opens the new segment files with `mmap`, and atomically swaps the reader's internal snapshot
pointer. Searchers acquired *before* the reload keep their own `Arc` references to the old
segment readers and continue to see the old data, undisturbed. Only searchers acquired
*after* the reload see the new snapshot. Old and new searchers can coexist simultaneously.

**How expensive is a reload?** The reload *operation itself* is very cheap. The steps are:

1. Read `meta.json` — a small JSON file, one disk read.
2. For each *new* segment, open its files and call `mmap` — this sets up virtual address
   mappings but does **not** read any index data from disk yet (demand paging, §2.4).
3. Atomically update the internal pointer. Segments shared with the previous snapshot reuse
   their `Arc`s — no files are reopened.

However, **the I/O cost is deferred, not eliminated.** The first query that touches a page
inside a freshly mmap'd segment triggers a page fault and a real disk read at that moment.
So:

- `reload()` itself: fast (syscalls only, no data I/O).
- First search after reload on a new segment: pays the cold page fault cost for every
  index page it needs — posting lists, term dictionary, fast fields, etc.
- Subsequent searches on the same segment: served from the OS page cache, fast.

The total disk I/O to read a segment is the same regardless of when you call `reload()`.
What mmap buys you is **laziness** — pages are loaded on demand, only the pages actually
needed by a given query, rather than the entire segment upfront. A query that touches only
a small fraction of the index pays only for those pages.

**Snapshot isolation**: once you hold a `Searcher`, the world it sees is frozen. A writer
can commit a million new documents while your query is running; your searcher will not see
any of them. The next searcher you acquire (after a reload) will.

**`DocAddress` is snapshot-scoped**: a `DocAddress` encodes a segment ID and a segment-local
document ID. It is only valid within the snapshot that produced it. Passing a `DocAddress`
from one `Searcher` to a different `Searcher` is undefined behaviour — the document ID may
map to a different document in the other snapshot, or to a deleted document.

### 2.3 How many segments does a commit create?

**One segment per active indexing thread.** If you create the writer with the default thread
count (one per logical CPU) and call `commit()` once, you get N new segments — one from each
thread. A single-threaded writer (`writer_with_num_threads(1)`) always produces exactly one
segment per commit.

```
writer_with_num_threads(1) + commit()  →  1 new segment
writer_with_num_threads(4) + commit()  →  up to 4 new segments
```

Threads that received no documents produce no segment, so the actual count can be less than
the thread count if the batch is small. Over time, many small commits accumulate many
segments; tantivy's background **merge policy** (§11 of the implementation tutorial)
periodically coalesces them to keep search fast.

### 2.4 Where does the index live — disk or memory?

**Both, simultaneously, via memory-mapped files (mmap).**

The authoritative data always lives on disk. The segment files (posting lists, fast fields,
term dictionary, docstore, …) are written once on commit and never modified in place.

When a `Searcher` is created, tantivy opens those files with `mmap` — a kernel mechanism
that maps the file's bytes directly into the process's virtual address space. From the
program's point of view the data looks like a byte slice in memory, but the OS only loads
the actual disk pages into physical RAM the first time each page is accessed (demand paging).
Pages that fit in the OS page cache are served from RAM on subsequent accesses.

```
Disk (segment files)
   ↕  mmap
Virtual address space of the process   ←── Searcher reads here
   ↕  demand paging (OS page cache)
Physical RAM
```

Practical consequences:

| What you observe | Why |
|-----------------|-----|
| A fresh index open is fast — no "loading" step. | mmap doesn't copy data; it just sets up address mappings. |
| First query over a cold index is slower. | The OS has to page in the touched file regions from disk. |
| Subsequent queries on the same data are fast. | Pages stay in the OS page cache as long as RAM is available. |
| Total index size can exceed available RAM. | Only the pages actually touched need to be in RAM at once. |
| Dropping a `Searcher` calls `munmap`. | The virtual mapping is released; the OS may evict those pages. |

For `Index::create_in_ram` (used in tests), the data is backed by an in-memory buffer
instead of disk files. There is no mmap; everything lives in heap-allocated RAM and vanishes
when the `Index` is dropped.

### 2.5 Operational guidelines

**How often should you commit?**

- **Never commit per document.** `commit()` fsyncs all segment files and updates `meta.json`
  on disk — it is orders of magnitude slower than `add_document`.
- **Batch at least thousands of documents per commit**, ideally tens or hundreds of thousands.
  The write buffer is the primary knob: a larger buffer means more documents in memory before
  flushing, producing fewer but larger segments and better search performance.
- The **minimum allowed memory budget is 15 MB per thread**, enforced at writer construction.
  (`src/indexer/index_writer.rs:32`: `MEMORY_BUDGET_NUM_BYTES_MIN = MARGIN_IN_BYTES * 15`
  where `MARGIN_IN_BYTES = 1_000_000` at line 29.) A practical starting point for
  throughput indexing is 200–500 MB total across all threads.
- Each indexing thread **auto-flushes a partial segment to disk** when its buffer exceeds
  `budget − 1 MB`, *without* making it searchable. You may see segment files appear before
  `commit()` — this is normal. They become searchable atomically only on commit.
  (`src/indexer/index_writer.rs:54–55` doc comment; flush check at line 195:
  `if mem_usage >= memory_budget - MARGIN_IN_BYTES`.)
- For **near-real-time search**, commit every 1–10 seconds and rely on
  `ReloadPolicy::OnCommitWithDelay`. The reader polls `meta.json` every **500 ms** in
  production (1 ms in tests) to detect new commits.
  (`src/directory/mmap_directory/file_watcher.rs:12`:
  `POLLING_INTERVAL = Duration::from_millis(if cfg!(test) { 1 } else { 500 })`.)
  Note: `OnCommitWithDelay` is the **default** reload policy when you call `index.reader()`
  without explicit configuration. (`src/reader/mod.rs:53`.)
- For **bulk ingestion**, commit every 500 K–1 M documents or whenever the write buffer fills
  naturally. Reload the reader once at the end.

**How often should you create a `Searcher`?**

- **Once per query/request.** Acquiring a `Searcher` increments a handful of `Arc` reference
  counts — there is no disk I/O and no locking beyond an atomic read. It is effectively free.
- Do not hold a `Searcher` longer than the request lasts. Long-lived searchers keep `Arc`
  counts on old segment readers above zero, preventing superseded segment files from being
  deleted from disk.
- Ensure the reader is reloaded (via `reader.reload()` or `OnCommitWithDelay`) before
  calling `reader.searcher()` if you want to see recent commits. A stale reader returns a
  stale snapshot regardless of how many times you call `searcher()`.

**Source-verified constants:**

| Constant | Value | Location |
|----------|-------|----------|
| `MARGIN_IN_BYTES` | 1 000 000 (≈ 1 MB) | `src/indexer/index_writer.rs:29` |
| `MEMORY_BUDGET_NUM_BYTES_MIN` | 15 × MARGIN = **15 MB** per thread | `src/indexer/index_writer.rs:32` |
| `MEMORY_BUDGET_NUM_BYTES_MAX` | `u32::MAX − MARGIN` (≈ 4 GB) per thread | `src/indexer/index_writer.rs:33` |
| Auto-flush trigger | `mem_usage >= budget − MARGIN_IN_BYTES` | `src/indexer/index_writer.rs:195` |
| Default reload policy | `ReloadPolicy::OnCommitWithDelay` | `src/reader/mod.rs:53` |
| `OnCommitWithDelay` polling interval | **500 ms** (1 ms in tests) | `src/directory/mmap_directory/file_watcher.rs:12` |

### 2.6 Lucene/tantivy for update-intensive workloads

Since tantivy follows the same segment architecture as Lucene, understanding how Lucene behaves
under heavy update loads is directly applicable. This section summarises what major engineering
teams have learned in production, with primary sources.

#### The fundamental tension: immutability vs updates

Lucene's core design is **append-only**: segments are immutable once written. An "update" is
actually a **delete + add**: the old document gets a tombstone bit set in a per-segment
bitset, and a new document is appended to a new segment. The tombstone approach is intentional
— rewriting an immutable segment in place to remove one document would be prohibitively
expensive — but it creates costs that accumulate under high update rates:

| Cost | Description |
|------|-------------|
| **Disk space** | Tombstoned documents occupy space until the segment is merged away. |
| **RAM** | Per-document structures (norms, field data) still consume memory for deleted docs. |
| **Search overhead** | Every query checks the deletion bitset for each candidate document. |
| **Write amplification** | Merges re-read and re-write all surviving documents. Benchmarks show ~13–15× write amplification under heavy update loads (TieredMergePolicy ≈ 13.64×, LogByteSizeMergePolicy ≈ 14.49×). |

> Source: [Elasticsearch Segment Merges Explained](https://medium.com/@shivam.agarwal.in/elasticsearch-navigating-lucene-segment-merges-9ed775bd45cb) — discusses write amplification measurements under update-heavy workloads.
> Source: [Lucene's Handling of Deleted Documents — Elastic Blog](https://www.elastic.co/blog/lucenes-handling-of-deleted-documents) — official explanation of tombstones, RAM and disk costs, and TieredMergePolicy behaviour.

#### Twitter's Earlybird — a canonical real-world example

Twitter built **Earlybird**, a real-time search system on top of Lucene, to index tweets as
they are posted (launched October 2010, described in a 2013 ICDE paper). It is the most
cited public example of Lucene under extremely high update rates.

Key numbers (from the Earlybird paper, indexed on a single partition):
- Indexed at **~2 200 tweets/s** average (during the 2011 launch period, 1.6 B queries/day).
- **Query latency**: 50 ms end-to-end, 95th-percentile < 100 ms on a fully loaded server
  (144 million tweets, ~5 000 QPS per server).
- **Indexing-to-searchable latency**: ~10 seconds end-to-end.

To reach these numbers at this update rate, Twitter had to **depart significantly from
vanilla Lucene**:

1. **Single in-memory segment per partition.** Instead of flushing many small segments,
   Earlybird keeps all recent tweets in one RAM segment. Searches only touch one segment —
   no per-segment overhead.
2. **Lock-free concurrent reads and writes.** Standard Lucene uses a delete bitset that
   requires locking to coordinate with readers. Earlybird replaces this with a lock-free
   structure so writers and readers never block each other.
3. **Append-only tweet updates.** Tweets rarely change content; most "updates" are new
   annotations (retweet counts, etc.) stored as mutable side data, not Lucene deletes.

> Source: [The Engineering Behind Twitter's New Search Experience (2011)](https://blog.twitter.com/engineering/en_us/a/2011/the-engineering-behind-twitter-s-new-search-experience) — Twitter Engineering Blog.
> Source: [Earlybird: Real-Time Search at Twitter — Stephen Holiday's notes](https://stephenholiday.com/notes/earlybird/) — concise summary of the ICDE 2013 paper with key numbers.
> Source: [How Twitter Uses Lucene for Real-Time Search — Lucidworks](https://lucidworks.com/post/how-twitter-uses-apache-lucene-for-real-time-search/) — architectural overview.

In 2022, Twitter also published how they used a Kafka-backed **Ingestion Service** to
decouple the write rate from Elasticsearch's indexing capacity under traffic spikes:

> Source: [Stability and Scalability for Search — Twitter Engineering Blog (2022)](https://blog.x.com/engineering/en_us/topics/infrastructure/2022/stability-and-scalability-for-search).

#### NRT performance benchmarks (Mike McCandless)

Mike McCandless (Apache Lucene committer) has maintained public nightly benchmarks since 2011.
The NRT benchmark simulates the hardest update pattern: delete+add at 1 MB plain text/s while
reopening a new NRT reader every second on the full English Wikipedia index.

Key findings:
- **NRT reopen latency** (time to make newly indexed docs searchable): ~43–52 ms on spinning
  disk, dramatically reduced variance on SSD.
- **Concurrent flushing** (introduced 2011-05-02): each indexing thread flushes its own
  segment independently without blocking other threads — a crucial throughput win on
  multi-core hardware.
- **NRT vs commit**: reopening an NRT reader is far cheaper than a full `commit()` because
  it does not fsync. Lucene separates the two: `refresh` (make docs searchable) vs `commit`
  (make docs durable). Tantivy currently folds both into `commit()`.

> Source: [Lucene's near-real-time search is fast! — Changing Bits (2011)](https://blog.mikemccandless.com/2011/06/lucenes-near-real-time-search-is-fast.html) — McCandless's original NRT benchmark post.
> Source: [Lucene nightly NRT latency benchmark](https://people.apache.org/~mikemccand/lucenebench/nrt.html) — live benchmark results.
> Source: [Lucene nightly benchmarks](https://benchmarks.mikemccandless.com/) — full suite.

#### Practical guidance for update-heavy tantivy applications

| Scenario | Recommendation |
|----------|---------------|
| **Low update rate** (< 100 updates/s) | Standard approach: delete by term + re-add. Merges will keep tombstone ratio under control automatically. |
| **Medium update rate** (100–10 000/s) | Commit frequently (every few seconds). Monitor segment count and tombstone ratio. SSD strongly recommended for NRT reopen latency. |
| **Very high update rate** (> 10 000/s) | Lucene/tantivy's general-purpose architecture starts to show strain. Consider: (1) partitioning by time (old segments become read-only, only the hot partition takes writes); (2) an intermediate queue (Kafka) to absorb bursts and smooth the write rate; (3) a purpose-built engine (e.g., a hybrid inverted+LSM design). |
| **Read-heavy, occasional updates** | Lucene/tantivy excels here. Bulk-load once, commit, then serve. Periodic merges keep the index fast. |

> Source: [Apache Lucene — ImproveIndexingSpeed wiki](https://cwiki.apache.org/confluence/display/LUCENE/ImproveIndexingSpeed) — official Lucene performance tuning guide.
> Source: [Visualizing Lucene's segment merges — Changing Bits](https://blog.mikemccandless.com/2011/02/visualizing-lucenes-segment-merges.html) — intuition for how merge policies behave over time.

### Questions

1. What is the role of a `Collector`? Name two built-in collectors.
2. You acquire a `Searcher`, then the writer commits 10 000 new documents. Does your existing
   `Searcher` see those documents? Why or why not?
3. Can you call `searcher.doc(addr)` with an address obtained from a *different* searcher
   snapshot? What can go wrong?
4. You call `writer.add_document(doc)` one million times, then your process crashes before
   `commit()`. What is the state of the index?
5. The writer is "multithreaded internally". If you call `writer_with_num_threads(4)` and
   do one commit, how many new segments are created?
6. Is it expensive to call `reader.searcher()` on every HTTP request? Why or why not?
7. You hold a `Searcher` from 10 minutes ago. The writer has since merged several segments.
   Can those old segment files be deleted from disk while your searcher is alive?

### Answers

1. A `Collector` defines what to do with documents that match the query — it consumes the
   scored document stream and accumulates a result. Two built-in collectors: `TopDocs`
   (returns the top-k highest-scoring documents) and `Count` (returns the total number of
   matching documents without fetching any of them).

2. No. Your existing `Searcher` is a frozen snapshot taken at the moment of the last reader
   reload *before* you called `searcher()`. A commit changes the on-disk state but does not
   affect live `Searcher` objects. The new documents become visible only to searchers acquired
   after the reader is reloaded (via `reader.reload()` or the background policy).

3. It is unsafe. A `DocAddress` encodes a segment ID and a segment-local document ID that are
   only meaningful within the snapshot that produced them. In a different snapshot, the same
   segment ID may not exist, or the same document ID within that segment may refer to a
   completely different document (e.g., if a merge renumbered documents). You will get
   wrong data or a panic.

4. The index is unchanged from the state of the last successful commit. All one million
   `add_document` calls lived in the in-memory write buffer and were never flushed to disk.
   Tantivy's commit is all-or-nothing: the only durable state is what `commit()` wrote.

5. Up to 4 new segments — one per thread. Threads that received no documents produce no
   segment, so the actual count can be less if the batch was small relative to the thread
   count. With a large batch that fills all threads evenly, you get exactly 4.

6. No — acquiring a `Searcher` is essentially free. It increments a few `Arc` reference
   counts and reads an atomic pointer; there is no disk I/O, no lock contention, and no
   data copying. Create one per request without hesitation.

7. No, they cannot be deleted yet. The `Searcher` holds `Arc` references to each segment
   reader it covers, including the old (pre-merge) segments. Tantivy only releases segment
   files once every `Arc` referencing them reaches zero. As long as your old `Searcher` is
   alive, the old segment files remain on disk. They are cleaned up the moment the last
   `Searcher` that references them is dropped.

---

## 3. The Schema

The schema is **fixed** the moment the index is created. You cannot add, remove or change
field types later. Design it carefully up front.

### 3.1 Field options

`STORED` and `FAST` apply to **all field types**. `TEXT` and `STRING` are specific to text
fields.

| Option | Applies to | What it enables |
|--------|-----------|----------------|
| `TEXT` | text | Tokenised, indexed with term frequencies and positions. Enables full-text search and phrase queries. |
| `STRING` | text | Indexed as one token (no tokenisation). Good for IDs, tags, enum values. Enables exact-match and `TermQuery`. |
| `STORED` | all | Raw value saved in a compressed row store. Required to retrieve the value in results via `searcher.doc()`. |
| `FAST` | all | Column-oriented storage with O(1) random access per `DocId`. Required for sorting, aggregations, and reading values during scoring. |
| `INDEXED` | numeric/bool/date/ip | Adds the value to the inverted index so range queries work. Numeric fields have this by default. |

Combine options with `|`:

```rust
schema_builder.add_text_field("title",  TEXT | STORED);  // full-text search + retrievable
schema_builder.add_text_field("body",   TEXT);            // full-text search, not retrievable
schema_builder.add_text_field("isbn",   STRING | STORED); // exact-match ID, retrievable
```

Key rules:
- A field with **only `STORED`** can be retrieved but **not searched**.
- A field with **only `TEXT`** can be searched but **its raw value cannot be retrieved** from
  the docstore; you will only see it in snippets.
- `FAST` is independent of `STORED`: you can have a fast field that is not stored and vice
  versa.

### Questions

1. You declare `add_text_field("body", TEXT)`. A user searches for "whale". Is the document
   returned? Can you print the body text from the result?
2. You want to sort results by price and also display the price in search results. Which
   options must `price` have?
3. What is the difference between `TEXT` and `STRING` for the value `"New York"`?
   What happens when you search for `"york"` on each?

### Answers

1. Yes — searching matches because `TEXT` indexes the field. But you cannot print the
   body: `TEXT` alone does not include `STORED`, so the original value is not in the
   docstore and `doc.get_first(body)` returns `None`. To both search and display, use
   `TEXT | STORED`.
2. `price` needs `FAST` (column store, for sorting and reading per `DocId`) and
   `STORED` (so the original value can be returned in the result). Typical declaration:
   `add_f64_field("price", FAST | STORED)`. Numeric fields don't need `INDEXED` for
   sorting alone; add it (`INDEXED | FAST | STORED`) only if you also want range
   queries through the inverted index.
3. `TEXT` runs the default tokeniser: `"New York"` becomes tokens `[new, york]`
   (lowercased), and a search for `"york"` matches. `STRING` indexes the whole value
   as a single untokenised, case-sensitive term `"New York"`; a search for `"york"`
   does **not** match — only the exact term `"New York"` does.

### Exercise A — Schema choices

For each field below, choose the right combination of options and justify:
- `article_id`: unique identifier, used only for deletion and deduplication.
- `content`: long article body, full-text searchable, no need to display it verbatim.
- `published_at`: datetime, used for date-range filtering and sorting by recency.
- `author_name`: full-text searchable, also displayed in results.
- `view_count`: used for sorting by popularity and for aggregations; never displayed.

---

### 3.1.1 Multi-valued STRING fields (tags)

A common case is a field that holds a small set of independent labels — tags, categories,
codes — where each label must be searchable as a whole token. `STRING` is the right
choice: it indexes the value as a single un-tokenised, case-preserving term, and tantivy
supports adding the same field multiple times per document.

```rust
let mut doc = TantivyDocument::default();
doc.add_text(tags, "rust");
doc.add_text(tags, "search-engine");
doc.add_text(tags, "Lucene-like");
```

Each `add_text` call appends another term to the `tags` posting list for that document.
A query for any single tag is an exact `TermQuery`:

```rust
TermQuery::new(
    Term::from_field_text(tags, "search-engine"),
    IndexRecordOption::Basic,
)
```

…or via the parser: `tags:search-engine`.

**Caveats specific to STRING:**

- **Exact and case-sensitive.** `tags:Rust` ≠ `tags:rust`. For case-insensitive matching,
  lowercase tags before indexing or use a custom analyser that runs only `LowerCaser`
  (treated as a TEXT field with `IndexRecordOption::Basic`).
- **No partial matches.** Substrings, prefixes, and whitespace splits don't work — the
  term is the whole tag. For prefix search use `RegexQuery` or a tokenised TEXT field.
- **Multi-valued = several terms in the same posting list.** A query for `"rust"`
  matches any document that added `"rust"` to `tags`, regardless of the other tags it
  carries.
- **Aggregations / facets / sort.** Add `FAST` (`STRING | FAST`) so each tag value can
  be read as a column entry per `DocId`. Without `FAST` you can search but not bucket.

If you want hierarchical tag navigation (e.g. `/lang/rust`, `/topic/search`), use a
**facet** field instead — `add_facet_field` is built for exactly that.

---

### 3.2 Tokenizers and token filters

When a `TEXT` field is indexed, the raw string is passed through a **text analyzer** — a
pipeline of one *tokenizer* (splits text into tokens) followed by zero or more *token
filters* (transform or discard tokens). The choice of analyzer is what makes `"running"` and
`"runs"` match the same query term, or makes accent-insensitive search work.

#### Built-in named analyzers

Three analyzers are registered by name out of the box:

| Name | What it does | When to use |
|------|-------------|-------------|
| `"default"` | Splits on whitespace and punctuation, removes tokens > 40 bytes, lowercases. | General-purpose text in English or Latin-script languages. |
| `"raw"` | No splitting — the entire field value is one token. | UUIDs, URLs, exact-match IDs (same effect as `STRING` but via the analyzer layer). |
| `"en_stem"` | Same as `default` + Porter stemmer for English. | English prose where recall matters more than precision. |
| `"whitespace"` | Splits on whitespace only; keeps punctuation attached to words. | Code, structured tags, or data where punctuation is significant. |

`TEXT` uses `"default"` unless you override it. `STRING` bypasses the analyzer entirely.

#### Tokenizer structs

You can also build a custom analyzer programmatically:

| Struct | Splits on | Notes |
|--------|-----------|-------|
| `SimpleTokenizer` | Whitespace and punctuation | The basis of `"default"` and `"en_stem"`. |
| `WhitespaceTokenizer` | Whitespace only | Keeps punctuation; basis of `"whitespace"`. |
| `RawTokenizer` | Nothing (whole value = one token) | Basis of `"raw"`. |
| `NgramTokenizer::new(min, max, prefix_only)` | Character n-grams | `new(2,3,false)` emits all bigrams and trigrams. `prefix_only=true` emits only prefix n-grams — good for autocomplete. |
| `RegexTokenizer` | Each match of a regex = one token | Useful for structured text (e.g., extract quoted strings). |

#### Token filters

Filters are chained after the tokenizer with `.filter(...)`:

| Filter | Effect |
|--------|--------|
| `LowerCaser` | Converts every token to lowercase. |
| `RemoveLongFilter::limit(n)` | Drops tokens longer than `n` bytes (default limit: 40). |
| `AlphaNumOnlyFilter` | Drops tokens that contain non-alphanumeric characters. |
| `AsciiFoldingFilter` | Maps accented characters to their ASCII base (`é→e`, `ü→u`). Enables accent-insensitive search. |
| `StopWordFilter::new(Language::English)` | Removes common stop words for the given language. |
| `StopWordFilter::remove(vec!["the","a"])` | Removes a custom list of stop words. |
| `Stemmer::new(Language::English)` | Applies a Snowball stemmer. Must be applied *after* `LowerCaser`. 18 languages supported (Arabic, Danish, Dutch, English, Finnish, French, German, Greek, Hungarian, Italian, Norwegian, Portuguese, Romanian, Russian, Spanish, Swedish, Tamil, Turkish). |
| `SplitCompoundWords` | Splits Germanic compound nouns into parts using a dictionary. |

#### Building and registering a custom analyzer

```rust
use tantivy::tokenizer::*;

// Build a French full-text analyzer
let fr_analyzer = TextAnalyzer::builder(SimpleTokenizer::default())
    .filter(RemoveLongFilter::limit(40))
    .filter(LowerCaser)
    .filter(AsciiFoldingFilter)
    .filter(StopWordFilter::new(Language::French).unwrap())
    .filter(Stemmer::new(Language::French))
    .build();

// Register it under a name
index.tokenizers().register("fr_stem", fr_analyzer);

// Use it in the schema
let title = schema_builder.add_text_field(
    "title",
    TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("fr_stem")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored(),
);
```

#### IndexRecordOption — what gets stored in the posting list

Each indexed text field records one of three levels of detail:

| Option | Stores | Cost | Use when |
|--------|--------|------|----------|
| `Basic` | DocIds only | Lowest | Filtering only — no BM25, no phrase queries. |
| `WithFreqs` | DocIds + term frequencies | Medium | BM25 scoring without phrase queries. |
| `WithFreqsAndPositions` | DocIds + freqs + positions | Highest | Full BM25 + phrase queries. **Default for `TEXT`.** |

`TEXT` defaults to `WithFreqsAndPositions`. Use `Basic` or `WithFreqs` only when you know
you will never run phrase queries on the field and want to save disk space and indexing time.

#### Questions

1. What is the difference between the `"default"` and `"whitespace"` tokenizers? Give an
   example where they produce different tokens.
2. You want to build an autocomplete field: as the user types `"hel"`, documents containing
   `"hello"` or `"help"` should match. Which tokenizer would you choose, and why?
3. You index the string `"café au lait"` with `AsciiFoldingFilter`. What tokens are produced?
   What query would then match the document?
4. The `Stemmer` filter must be applied after `LowerCaser`. Why?
5. A field is declared with `IndexRecordOption::Basic`. Can you run a phrase query on it?
   Can you score results with BM25?
6. You have a `"tags"` field that stores values like `"machine-learning"` and you want an
   exact-match search on the full tag. Which built-in analyzer is most appropriate?

#### Answers

1. `"default"` splits on both whitespace *and* punctuation, so `"don't"` becomes `["don",
   "t"]`. `"whitespace"` splits on whitespace only, so `"don't"` stays as one token
   `"don't"`. For a field like `["tag-a", "tag-b"]`, `"default"` would also split on the
   hyphen; `"whitespace"` would not.

2. `NgramTokenizer::new(2, 4, true)` (prefix n-grams). With `prefix_only=true`, `"hello"`
   emits `"he"`, `"hel"`, `"hell"`, `"hell"` — so a query for `"hel"` matches because
   `"hel"` is one of the indexed tokens. All-ngrams (`prefix_only=false`) would also work
   but index far more tokens.

3. `AsciiFoldingFilter` converts `é→e`, so the tokens are `["cafe", "au", "lait"]` (after
   also passing through `LowerCaser`). A query for `"cafe"` or `"café"` would match because
   the query is run through the same analyzer at search time.

4. The stemmer expects lowercase input — Snowball algorithms are defined for lowercase text.
   If `"Running"` is passed to the stemmer before lowercasing, many stemmers fail to
   recognise it. Lowercase first, stem second.

5. Phrase queries require positions — they cannot run on a `Basic` field. BM25 requires term
   frequencies — it cannot score accurately on a `Basic` field either (tantivy falls back to
   treating every match as TF=1, which degrades ranking quality).

6. `"raw"` — it treats the entire value as a single token with no transformation, so
   `"machine-learning"` is indexed as the single term `machine-learning` and only an exact
   query for that string will match.

---

### 3.3 Non-text field types

| Method | Type | Notes |
|--------|------|-------|
| `add_u64_field` | unsigned integer | Counts, IDs, timestamps as epoch ms. |
| `add_i64_field` | signed integer | Scores, offsets, temperatures — anything that can be negative. |
| `add_f64_field` | 64-bit float | Prices, ratings, continuous values. |
| `add_date_field` | datetime | Wraps `tantivy::DateTime`. Stored internally as microseconds since epoch. |
| `add_bool_field` | boolean | True/false flag. Indexed, so you can filter on it. |
| `add_ip_addr_field` | IPv6 address | IPv4 stored as IPv4-mapped IPv6. |
| `add_bytes_field` | raw bytes | Binary payloads — e.g. serialised feature vectors for learning-to-rank. |
| `add_json_field` | JSON object | Indexes fields inside a JSON blob dynamically without a fixed sub-schema. |
| `add_facet_field` | hierarchical path | **Not numeric.** Special type for categorical navigation. Values: `/Electronics/Laptops`. See §9. |

Numeric, date, bool and IP fields are **indexed by default** — range queries work without
any extra option. `FAST` must be added explicitly when you need sorting or aggregations.

```rust
schema_builder.add_u64_field("views",      FAST | STORED);
schema_builder.add_f64_field("price",      FAST | STORED | INDEXED);
schema_builder.add_date_field("created",   FAST | STORED);
schema_builder.add_bool_field("active",    FAST | STORED);
schema_builder.add_facet_field("category", FacetOptions::default());
```

### Questions

1. You have a `timestamp_ms: u64` field with only `FAST | STORED`. Can you do a range query
   `timestamp_ms > 1700000000000`? What option is missing?
2. What is the difference between `add_bytes_field` and `add_json_field`?
3. Can you use `add_text_field` with `STRING` for a field you also want to aggregate on?
   What is missing?

### Answers

1. No — `FAST | STORED` lets you sort by the value and return it, but range queries
   need the **inverted index**. Add `INDEXED`: `NumericOptions::default().set_indexed()
   .set_fast().set_stored()` (or the equivalent shorthand). Without `INDEXED`, the
   term dictionary has no entries for `timestamp_ms` and the byte-range scan that
   powers a `RangeQuery` finds nothing.
2. `add_bytes_field` stores opaque `Vec<u8>` blobs (e.g. embeddings, IDs, image
   hashes) — no parsing, no inverted-index expansion, just a typed raw byte field
   you can index/store/fast. `add_json_field` stores structured JSON: tantivy walks
   the object and indexes each leaf as a sub-field with type detection (text,
   numeric, bool), so you can query nested paths like `meta.user.id` without
   declaring each leaf in the schema.
3. Yes — `STRING` is fine for aggregation buckets (you usually *want* the value
   un-tokenised so `"New York"` stays one bucket). What's missing is `FAST`:
   aggregations read column values per `DocId` and require the field to be a fast
   field. Use `STRING | FAST` (and add `STORED` if you also want to display it).

---

### 3.4 Full schema example

```rust
use tantivy::schema::*;

let mut sb = Schema::builder();
let id       = sb.add_text_field("id",       STRING | STORED);
let title    = sb.add_text_field("title",    TEXT | STORED);
let body     = sb.add_text_field("body",     TEXT);
let price    = sb.add_f64_field("price",     FAST | STORED);
let rating   = sb.add_f64_field("rating",    FAST | STORED);
let active   = sb.add_bool_field("active",   FAST | STORED);
let category = sb.add_facet_field("category", FacetOptions::default());
let schema   = sb.build();
```

`add_*_field` returns a `Field` handle. Keep these — you need them to build documents,
terms, and queries.

---

## 4. Creating an Index

```rust
use tantivy::Index;

// Persistent — stores files in a directory
let index = Index::create_in_dir("/path/to/index", schema.clone())?;

// Open an existing index (schema is loaded from meta.json)
let index = Index::open_in_dir("/path/to/index")?;

// Ephemeral — lives in RAM only, useful for tests
let index = Index::create_in_ram(schema.clone());
```

When you create an index, tantivy writes a `meta.json` into the directory. This file records
the schema and the list of segments. You never need to edit it manually.

### Questions

1. You call `Index::create_in_dir` on a directory that already contains an index. What
   happens? (Check the API.)
2. You build an in-RAM index for a unit test. After the test, is the data persisted anywhere?
3. What file does tantivy write first when creating a new index?

### Answers

1. `create_in_dir` fails (returns `Err`) if the directory already contains an index —
   it refuses to clobber existing data. Use `Index::open_in_dir` to attach to an
   existing index, or `Index::open_or_create` to do whichever is appropriate.
2. No. A `RamDirectory` lives entirely in process memory; when the test process exits
   the index is gone. For a persistent test artifact, use `MmapDirectory::open(path)`
   instead.
3. `meta.json` — the empty index manifest, written atomically. Without it,
   `Index::open_in_dir` cannot recognise the directory as an index. As segments are
   later flushed, segment files appear and `meta.json` is rewritten (atomically) to
   reference them.

---

## 5. Indexing Documents

### 5.1 The IndexWriter

```rust
// 50 MB write buffer — a good starting point.
// Larger budget → more docs buffered before flushing → higher throughput.
let mut writer: IndexWriter = index.writer(50_000_000)?;
```

**There can be at most one `IndexWriter` per index at a time**, even across processes. Tantivy
uses a file lock (`meta.lock`) to enforce this. If a second writer tries to acquire the lock,
it will fail immediately with an error.

The writer is multithreaded internally: it spawns one indexing thread per CPU core
(configurable with `writer_with_num_threads`). Each thread independently builds its own
in-memory buffer and flushes one segment on commit.

### 5.2 Building and adding documents

Two equivalent styles:

```rust
// Style 1: builder
let mut doc = TantivyDocument::default();
doc.add_text(title, "The Old Man and the Sea");
doc.add_text(id,    "978-0684801223");
doc.add_f64(price,  12.99);
writer.add_document(doc)?;

// Style 2: doc! macro
writer.add_document(doc!(
    title => "Of Mice and Men",
    id    => "978-0140177398",
    price => 8.99_f64,
))?;
```

A field can carry **multiple values** — just repeat the key:

```rust
writer.add_document(doc!(
    title => "Frankenstein",
    title => "The Modern Prometheus",   // second title on the same document
    id    => "978-9176370711",
))?;
```

`add_document` is non-blocking: it enqueues the document into the writer's internal buffer.
The document is not on disk yet.

### 5.3 Committing

```rust
writer.commit()?;  // blocking — flushes all buffered documents to disk
```

After a successful commit:
- All buffered documents are written to one or more immutable segment files.
- `meta.json` is updated atomically.
- The data is durable: a crash after `commit()` will not lose documents.
- A crash **before** `commit()` rolls back to the previous commit state.

You can commit multiple times. Each commit creates new segment files. Tantivy automatically
merges small segments in background threads.

### 5.4 Deleting documents

Tantivy has no primary key concept. You delete by **term** — any indexed field value that
uniquely identifies the document:

```rust
let term = Term::from_field_text(id, "978-9176370711");
writer.delete_term(term);
writer.commit()?;
```

Deletion is not immediate removal. At commit time, tantivy locates all segments containing
documents that match the term and marks those `DocId`s as deleted in an alive bitset (`.del`
file). The bytes are reclaimed only when the segment is eventually merged.

### 5.5 Updating documents

There is no update operation. Update = delete + re-insert:

```rust
writer.delete_term(Term::from_field_text(id, "978-9176370711"));
writer.add_document(doc!(
    title => "Frankenstein",    // corrected spelling
    id    => "978-9176370711",
    price => 9.99_f64,
))?;
writer.commit()?;
```

Between the commit that deletes the old version and the commit that adds the new one,
searchers holding a snapshot may briefly see neither or both, depending on timing. Within a
single commit, delete + insert is atomic: the old version disappears and the new one appears
together.

### Questions

1. You call `add_document()` 5000 times without calling `commit()`. Are those documents
   searchable? What happens if the process is killed?
2. You have two JVM processes both calling `Index::open_in_dir` and then `.writer(50_000_000)`
   on the same directory. What happens to the second call?
3. You call `delete_term(t)` and immediately call `reader.searcher()`. Will the deleted
   document be absent from the new searcher's results?
4. A document has two `title` values: "Frankenstein" and "The Modern Prometheus". You call
   `delete_term(Term::from_field_text(title, "Frankenstein"))`. Is the document deleted?

### Answers

1. No, they are not searchable yet — they live in the writer's in-memory buffer and
   in a per-thread WAL-style state, but no segment exists for any reader to attach to.
   If the process is killed, the buffered docs are lost: the index reverts to the last
   committed state (commit is the only durability boundary).
2. Tantivy holds a process-level **directory lock** (`.tantivy-writer.lock`) on the
   directory. The second `.writer(...)` call fails with a lock error — only one
   `IndexWriter` can be open against a given directory at a time, regardless of which
   process is asking.
3. No. `delete_term` only registers a tombstone in the writer's pending state; the
   delete becomes durable on `commit()`, and the deletion becomes visible to readers
   only after the next reader reload (manual `reload()` or the background policy).
   Acquiring a `Searcher` between `delete_term` and `commit` returns the previous
   snapshot in which the doc is still alive.
4. Yes. `delete_term` deletes every document containing the given term in the given
   field — the title field has both "Frankenstein" and "The Modern Prometheus" as
   indexed terms (well, after tokenisation, the individual tokens), so the doc
   matches and is tombstoned. There is no "match all values" semantics — one matching
   term is enough. This is why for delete-by-key you should use a `STRING` field with
   a unique id and call `delete_term` against that id.

### Exercise B — Index and commit lifecycle

Write a Rust program that:
1. Creates an in-RAM index with fields: `id` (STRING), `title` (TEXT | STORED), `year` (u64, FAST | STORED).
2. Adds 5 books.
3. Commits.
4. Searches for any keyword and prints titles and scores.
5. Deletes one book by its `id`.
6. Adds a corrected version of that book.
7. Commits and reloads the reader.
8. Searches again and confirms the old version is gone and the new one appears.

---

## 6. Searching

### 6.1 IndexReader and Searcher

```rust
use tantivy::ReloadPolicy;

// Create once; live for the entire process lifetime.
let reader = index
    .reader_builder()
    .reload_policy(ReloadPolicy::OnCommitWithDelay)
    .try_into()?;

// Acquire a fresh snapshot for each request — very cheap.
let searcher = reader.searcher();
```

`ReloadPolicy::OnCommitWithDelay` makes the reader automatically detect new commits in the
background and make them available via the next `reader.searcher()` call. You can also
trigger a reload manually: `reader.reload()?`.

A `Searcher` is a **frozen snapshot** of the index. It holds references to the segment files
that existed at the moment it was acquired. No subsequent commit can change what this
searcher sees. This means:
- Multi-query sessions are consistent: every query in the same request runs on the same data.
- A searcher can outlive commits safely.
- You should release (drop) the searcher when the request is done. Holding one indefinitely
  prevents garbage collection of old segment files.

```rust
searcher.num_docs()          // total live documents across all segments
searcher.segment_readers()   // one SegmentReader per segment
```

### 6.2 The QueryParser — user-facing queries

`QueryParser` translates a human-readable query string into a `Query` object.

```rust
use tantivy::query::QueryParser;

// Default fields searched when the user doesn't specify one.
let qp = QueryParser::for_index(&index, vec![title, body]);

let q = qp.parse_query("sea whale")?;           // OR: any doc with "sea" or "whale"
let q = qp.parse_query("sea AND whale")?;       // both terms required
let q = qp.parse_query("sea -storm")?;          // "sea" but NOT "storm"
let q = qp.parse_query("title:sea")?;           // field-specific
let q = qp.parse_query("title:sea^2 body:sea")?; // title match worth 2×
let q = qp.parse_query("\"old man sea\"")?;     // phrase: exact sequence
let q = qp.parse_query("pri*")?;               // prefix query
```

The query parser is best for user-facing search boxes. For internal programmatic queries,
prefer constructing query objects directly (§6.3) — it is faster and safer.

### 6.3 Programmatic queries

All query types live in `tantivy::query`. Here is the complete set.

#### TermQuery — exact term match

```rust
use tantivy::query::TermQuery;
use tantivy::schema::IndexRecordOption;

let q = TermQuery::new(
    Term::from_field_text(title, "whale"),
    IndexRecordOption::Basic,   // only DocIds, no freq/positions
);
```

`IndexRecordOption::Basic` is sufficient for filtering. Use `WithFreqs` or
`WithFreqsAndPositions` when BM25 scoring or phrase detection is needed.

#### TermSetQuery — match any of a set of terms

More efficient than many `Should` TermQuery clauses when matching a large set of known values
(e.g. a blocklist of IDs, or a "find all of these authors" query).

```rust
use tantivy::query::TermSetQuery;

let q = TermSetQuery::new(vec![
    Term::from_field_text(author, "hemingway"),
    Term::from_field_text(author, "steinbeck"),
    Term::from_field_text(author, "faulkner"),
]);
```

#### BooleanQuery — combine queries

```rust
use tantivy::query::{BooleanQuery, Occur};

let q = BooleanQuery::new(vec![
    (Occur::Must,    Box::new(tq1)),   // document must match
    (Occur::Should,  Box::new(tq2)),   // matching boosts score, not required
    (Occur::MustNot, Box::new(tq3)),   // document must NOT match
]);
```

`Should` clauses: at least one must match when there are no `Must` clauses. If `Must` clauses
exist, `Should` clauses are purely for scoring.

#### ConstScoreQuery — filter without scoring

Wraps any query and replaces its BM25 score with a fixed constant. Use this when you want to
apply a query purely as a filter (e.g. in a `Must` boolean clause) without letting it affect
the final ranking score.

```rust
use tantivy::query::ConstScoreQuery;

// "active = true" filter that contributes 0.0 to the score
let filter = ConstScoreQuery::new(
    Box::new(TermQuery::new(Term::from_field_bool(active, true), IndexRecordOption::Basic)),
    0.0,
);
```

#### DisjunctionMaxQuery — best-field scoring

Like `BooleanQuery` with `Should`, but instead of summing sub-query scores it takes the
**maximum** score among matching sub-queries. A small `tie_breaking_boost` is added for
documents that match more than one clause. This avoids over-rewarding documents that match
the same query term in many fields.

```rust
use tantivy::query::DisjunctionMaxQuery;

let q = DisjunctionMaxQuery::with_tie_breaking_boost(
    vec![
        Box::new(TermQuery::new(Term::from_field_text(title, "rust"), IndexRecordOption::WithFreqs)),
        Box::new(TermQuery::new(Term::from_field_text(body,  "rust"), IndexRecordOption::WithFreqs)),
    ],
    0.1,  // tie-breaking boost for multi-field matches
);
```

Use `DisjunctionMaxQuery` instead of `BooleanQuery(Should)` when you want "best field wins"
semantics rather than score accumulation.

#### RangeQuery — numeric / date / bool ranges

Numeric values are encoded big-endian so a numeric range becomes a byte-range scan of the
term dictionary.

```rust
use tantivy::query::RangeQuery;
use std::ops::Bound;

// f64 range: 10.0 ≤ price ≤ 50.0
let q = RangeQuery::new(
    Bound::Included(Term::from_field_f64(price, 10.0)),
    Bound::Included(Term::from_field_f64(price, 50.0)),
);

// u64 open-ended: timestamp after 2020-01-01 (ms)
let q = RangeQuery::new(
    Bound::Included(Term::from_field_u64(ts, 1_577_836_800_000u64)),
    Bound::Unbounded,
);

// Exclusive upper bound
let q = RangeQuery::new(
    Bound::Included(Term::from_field_f64(rating, 1.0)),
    Bound::Excluded(Term::from_field_f64(rating, 3.0)),
);
```

#### ExistsQuery — field presence

Matches all documents where a given field has at least one indexed value. Useful to filter
out documents with missing optional fields.

```rust
use tantivy::query::ExistsQuery;

// All documents that have any value in the "price" field
let q = ExistsQuery::new_exists_query("price".to_string());
```

#### FuzzyTermQuery — approximate spelling

```rust
use tantivy::query::FuzzyTermQuery;

let q = FuzzyTermQuery::new(
    Term::from_field_text(title, "whail"),
    1,     // max Levenshtein edit distance (insertions, deletions, substitutions)
    true,  // also match as prefix (last char may be incomplete)
);
```

#### PhraseQuery — ordered sequence of terms

```rust
use tantivy::query::PhraseQuery;

let q = PhraseQuery::new(vec![
    Term::from_field_text(body, "quick"),
    Term::from_field_text(body, "brown"),
    Term::from_field_text(body, "fox"),
]);
// matches "quick brown fox" but NOT "quick red fox" or "fox brown quick"
```

Requires the field to be indexed with `TEXT` (positions stored).

#### PhrasePrefixQuery — phrase with prefix on last term

Like `PhraseQuery` but the last term is treated as a prefix — useful for autocomplete.

```rust
use tantivy::query::PhrasePrefixQuery;

// matches "machine learn...", "machine learner", "machine learning", etc.
let q = PhrasePrefixQuery::new(vec![
    Term::from_field_text(body, "machine"),
    Term::from_field_text(body, "learn"),   // prefix
]);
```

#### RegexQuery — regular expression on a text field

Matches all terms in a field whose string representation matches a regex. Can be slow on
large vocabularies; prefer prefix or fuzzy queries for autocomplete.

```rust
use tantivy::query::RegexQuery;

// All documents with a title term matching /^whale.*/
let q = RegexQuery::from_pattern("whale.*", title)?;
```

#### AllQuery and EmptyQuery — match everything / nothing

```rust
use tantivy::query::{AllQuery, EmptyQuery};

// Matches every document in the index (score = 1.0 for all)
searcher.search(&AllQuery, &Count)?;

// Matches no document — useful as a placeholder
searcher.search(&EmptyQuery, &TopDocs::with_limit(10).order_by_score())?;
```

#### MoreLikeThisQuery — find similar documents

Given the content of a document (or a set of field values), finds other documents that are
similar. It works by extracting the most discriminating terms from the input and running a
`BooleanQuery(Should)` with those terms.

```rust
use tantivy::query::MoreLikeThisQuery;

let q = MoreLikeThisQuery::builder()
    .with_min_doc_frequency(2)      // ignore terms appearing in fewer than 2 docs
    .with_min_term_frequency(1)     // ignore terms appearing fewer than 1× in input
    .with_document(doc_address);    // seed document

let similar_docs = searcher.search(&q, &TopDocs::with_limit(5).order_by_score())?;
```

#### BoostQuery — scale a query's score

```rust
use tantivy::query::BoostQuery;

// Title matches count 3× more than body matches
let q = BooleanQuery::new(vec![
    (Occur::Should, Box::new(BoostQuery::new(
        Box::new(TermQuery::new(Term::from_field_text(title, "rust"), IndexRecordOption::WithFreqs)),
        3.0,
    ))),
    (Occur::Should, Box::new(TermQuery::new(
        Term::from_field_text(body, "rust"), IndexRecordOption::WithFreqs,
    ))),
]);
```

### Query type summary

| Query | Use case |
|-------|----------|
| `TermQuery` | Exact match on one term |
| `TermSetQuery` | Match any of a list of terms (multi-value IN) |
| `BooleanQuery` | Combine queries with Must/Should/MustNot |
| `ConstScoreQuery` | Apply a query as a pure filter (score = constant) |
| `DisjunctionMaxQuery` | Best-field scoring across multiple fields |
| `RangeQuery` | Numeric / date / bool range |
| `ExistsQuery` | Field presence check |
| `FuzzyTermQuery` | Approximate / typo-tolerant term match |
| `PhraseQuery` | Exact ordered sequence of terms |
| `PhrasePrefixQuery` | Phrase with prefix on last term (autocomplete) |
| `RegexQuery` | Regex match over the term vocabulary |
| `AllQuery` | Match every document |
| `EmptyQuery` | Match no document |
| `MoreLikeThisQuery` | Similarity search seeded by a document |
| `BoostQuery` | Scale another query's score by a factor |

### Questions

1. What is `Occur::Should` in a `BooleanQuery`? If all clauses are `Should`, is a document
   required to match at least one?
2. `FuzzyTermQuery::new(term, 2, true)` — what does the `true` mean? What does distance `2`
   allow in terms of character edits?
3. You want to find documents where `price >= 5.0 AND price <= 20.0`. Write the `RangeQuery`.
4. A user types `"quick brown"` in the search box. `QueryParser` parses this as OR.
   How do you make it match only documents where both words appear in order?
5. When would you prefer `DisjunctionMaxQuery` over `BooleanQuery(Should)` for a multi-field
   search? What problem does it solve?
6. What is the difference between `ConstScoreQuery(q, 0.0)` and `MustNot` in a BooleanQuery?

### Answers

1. `Should` means "may match"; the document is *not* required to match it
   individually. With **only** `Should` clauses, however, BooleanQuery degenerates
   into "match at least one" — a doc matching zero `Should`s is excluded. Mix in
   `Must` or `MustNot` and the `Should`s become purely additive (they boost the score
   when present but no longer gate matching).
2. The `2` is the maximum **Levenshtein edit distance** — up to 2 single-character
   insertions, deletions, or substitutions to reach an indexed term (e.g. `whale` ↔
   `whales` is 1, `whale` ↔ `wales` is 2). The `true` is `transposition_cost_one`:
   adjacent-character swaps count as a single edit (Damerau-Levenshtein) instead of
   two.
3. ```rust
   use std::ops::Bound;
   use tantivy::query::RangeQuery;
   use tantivy::schema::Type;
   use tantivy::Term;

   RangeQuery::new(
       Bound::Included(Term::from_field_f64(price, 5.0)),
       Bound::Included(Term::from_field_f64(price, 20.0)),
   )
   ```
4. Wrap the words in double quotes inside the parser input — `"\"quick brown\""`.
   That tells `QueryParser` to build a `PhraseQuery` for the `default_field` (or
   construct one programmatically with `PhraseQuery::new(vec![Term::from_field_text
   (field, "quick"), Term::from_field_text(field, "brown")])`).
5. Prefer `DisjunctionMaxQuery` for *the same query, multiple fields* (title vs body
   vs tags). `BooleanQuery(Should)` **adds** the per-field BM25 scores, which double-
   counts and rewards docs whose match is split mediocrely across fields.
   `DisjunctionMaxQuery` returns `max(scores) + tie_breaker × Σ(others)` — the doc
   gets the score of its *best* field, with a small bonus for additional matches.
   This avoids the "best of any one field" loss that plain disjunction suffers.
6. `ConstScoreQuery(q, 0.0)` *includes* matching docs with score 0; `MustNot` *excludes*
   them. The first is "match this but contribute nothing to ranking"; the second is
   "remove these docs entirely from the result set".

### Exercise C — Query construction

Write a function `fn search_books(searcher: &Searcher, keyword: &str, min_price: f64, max_price: f64)`
that returns the top 10 books matching:
- The keyword in `title` or `body` using `DisjunctionMaxQuery` (title boost 2.0), **AND**
- `price` between `min_price` and `max_price` inclusive (as a `ConstScoreQuery` filter).

Use `BooleanQuery` with `Occur::Must` for both sub-queries. Print title, price, and score.

---

### 6.4 Collectors

A collector receives each matched `DocId` and decides what to do with it.

#### TopDocs — top-k retrieval

```rust
use tantivy::collector::TopDocs;
use tantivy::Order;

// Top 10 by BM25 score
let hits: Vec<(f32, DocAddress)> =
    searcher.search(&query, &TopDocs::with_limit(10).order_by_score())?;

// Top 10 by f64 fast field, descending
let hits: Vec<(Option<f64>, DocAddress)> =
    searcher.search(&query, &TopDocs::with_limit(10)
        .order_by_fast_field::<f64>("price", Order::Desc))?;

// Top 10 by u64 fast field, ascending
let hits: Vec<(Option<u64>, DocAddress)> =
    searcher.search(&query, &TopDocs::with_limit(10)
        .order_by_fast_field::<u64>("timestamp_ms", Order::Asc))?;
```

When ordering by a fast field the "score" in the pair is `Option<T>` (the field value).
`None` means the document had no value for that field.

#### Pagination with and_offset

`and_offset(n)` skips the first `n` results. Use `with_limit` + `and_offset` for page-based
pagination. Note: tantivy must score **all** results up to `offset + limit` to find the
correct page, so deep pagination (high offset) is expensive.

```rust
let page  = 2usize;
let per_page = 10usize;

let hits = searcher.search(
    &query,
    &TopDocs::with_limit(per_page)
        .and_offset(page * per_page)
        .order_by_score(),
)?;
```

#### Custom scoring with tweak_score

`tweak_score` lets you combine BM25 with any fast field value — the standard way to
implement "popularity boost", "recency boost", or any learned ranking signal without
implementing a full custom query.

The argument is a **two-level closure**:
- Outer closure: called once per segment; opens fast field readers.
- Inner closure: called once per matched DocId; returns the final sort key.

```rust
use tantivy::SegmentReader;

let top_docs = TopDocs::with_limit(10).tweak_score(
    move |segment_reader: &SegmentReader| {
        // Open the fast field once for this segment
        let popularity = segment_reader
            .fast_fields()
            .u64("helpful_vote")
            .unwrap()
            .first_or_default_col(0);

        // Return the per-document scoring closure
        move |doc_id, bm25_score: f32| {
            let votes = popularity.get_val(doc_id);
            let popularity_boost = ((2 + votes) as f32).log2();
            bm25_score * popularity_boost
        }
    }
);

let hits: Vec<(f32, DocAddress)> = searcher.search(&query, &top_docs)?;
```

The field used in `tweak_score` must be `FAST`.

#### Count — just count matches

```rust
use tantivy::collector::Count;
let n: usize = searcher.search(&query, &Count)?;
```

#### MultiCollector — run several collectors in one pass

Avoids scoring documents twice when you need both the top results and a count.

```rust
use tantivy::collector::MultiCollector;

let mut mc = MultiCollector::new();
let top_handle   = mc.add_collector(TopDocs::with_limit(10).order_by_score());
let count_handle = mc.add_collector(Count);

let mut results  = searcher.search(&query, &mc)?;
let top_docs     = top_handle.extract(&mut results);
let count        = count_handle.extract(&mut results);
```

#### Count — just count matches

```rust
use tantivy::collector::Count;
let n: usize = searcher.search(&query, &Count)?;
```

#### MultiCollector — run several collectors in one pass

```rust
use tantivy::collector::MultiCollector;

let mut mc = MultiCollector::new();
let top_handle   = mc.add_collector(TopDocs::with_limit(10).order_by_score());
let count_handle = mc.add_collector(Count);

let mut results = searcher.search(&query, &mc)?;
let top_docs = top_handle.extract(&mut results);
let count    = count_handle.extract(&mut results);
```

### Questions

1. You use `order_by_fast_field::<f64>("price", Order::Asc)`. What type is the score in
   the returned `Vec`? What does `None` mean?
2. You want both the top 10 results and the total count in one search call. Which collector
   do you use?
3. A document does not have the `price` fast field set. Where does it appear when sorting by
   price ascending?
4. You want to implement "page 3, 10 results per page". Write the `TopDocs` call.
5. `tweak_score` uses a two-level closure. Why two levels? What work happens in the outer
   closure vs the inner closure?

### Answers

1. The score field becomes `Option<f64>` (the type parameter of the order-by-fast-field
   collector). `None` means the document does not have a value for the `price` fast
   field — typically because the field was not set at indexing time. With `Order::Asc`,
   tantivy's fast-field collector treats missing values according to the API's null
   policy; usually they sort at one end of the range, but rely on the type — `None`
   in your `Vec` is the explicit signal "no price for this doc".
2. `MultiCollector`. Register both `TopDocs::with_limit(10)` and `Count`, run a single
   `searcher.search(&q, &multi)`, then unpack the two handles. One pass over the
   matching docs feeds both collectors.
3. With `Order::Asc`, missing values typically sort **last** (effectively `+∞` so
   present values come first). Verify with the variant of `order_by_fast_field` you
   call — there are knobs to flip this — but the safe assumption is "absent = pushed
   to the end on ascending sort".
4. ```rust
   TopDocs::with_limit(10).and_offset(20)   // page 3 → skip 20, take 10
   ```
5. The outer closure runs **once per segment** and returns the inner closure; this is
   where you open expensive per-segment readers (fast-field readers, alive bitset,
   stored-field handle). The inner closure runs **once per matched DocId in that
   segment** and computes the tweaked score using the readers captured by the outer
   one. Two levels = pay segment-setup cost N times, not N × matches times.

---

### 6.5 Scoring and BM25

Every document matching a query gets a floating-point **score**. The default is **BM25**
(Best Match 25), whose formula for a single query term is:

```
score(d, t) = IDF(t) × TF_norm(d, t)

IDF(t) = ln(1 + (N - df + 0.5) / (df + 0.5))
           where N = total documents, df = documents containing term t

TF_norm(d, t) = tf / (tf + K1 × (1 - B + B × dl / avgdl))
                  where tf = term frequency in document d
                        dl = length of the field in d (number of tokens)
                     avgdl = average field length across all documents
                        K1 = 1.2  (controls TF saturation)
                         B = 0.75 (controls length normalisation)
```

Intuitions:
- **IDF**: a term appearing in few documents is highly discriminating. "Fulvous" (rare) →
  high IDF. "The" (ubiquitous) → IDF ≈ 0.
- **TF saturation**: doubling the term frequency does not double the score. The formula
  saturates — the k1 parameter controls how quickly.
- **Length normalisation**: a short field matching the query term is worth more than a long
  field with the same term frequency. Matching "whale" in a 5-word title scores higher than
  matching it once in a 1000-word body.

For multi-term queries the scores for individual terms are summed.

**Inspecting the score:**

```rust
let explanation = query.explain(&searcher, doc_address)?;
println!("{}", explanation.to_pretty_json());
// Shows IDF, TF, fieldnorm contribution for each matched term.
```

**Boosting:**

```rust
use tantivy::query::BoostQuery;
let boosted = BoostQuery::new(Box::new(term_query), 2.0);
```

Or via the query parser: `title:whale^3 body:whale`.

### Questions

1. Two documents both contain the word "ocean" once. Document A has a 5-word title;
   Document B has a 500-word body. Which one scores higher for the query `ocean`? Why?
2. The query is `"sea AND whale"`. How is the final BM25 score computed from the individual
   term scores?
3. IDF for the term "the" is nearly zero. Why is that? What does it mean for search results?
4. You want title matches to count 3× more than body matches. How do you express this with
   the `QueryParser`?

### Answers

1. **Document A.** BM25's length-normalisation factor `1 - B + B × dl/avgdl` shrinks
   the contribution from longer documents. A 5-word title has a tiny `dl`, so its TF
   contribution is barely discounted; a 500-word body has `dl ≫ avgdl`, so the same
   single occurrence is heavily damped. Plus, average title lengths are typically
   smaller, lowering `avgdl` for that field.
2. The Boolean AND scorer **sums** the per-term BM25 contributions:
   `score(d) = bm25(sea, d) + bm25(whale, d)`. Each term is scored independently
   against the doc, then added; there is no cross-term interaction in the formula.
3. `IDF = ln(1 + (N - n + 0.5) / (n + 0.5))`. For "the", `n ≈ N`, so the argument is
   ≈ `1`, and `ln(1) = 0`. A near-zero IDF means matching "the" contributes almost
   nothing to the score — the BM25 way of automatically downweighting stop words
   without an explicit stop-word list.
4. Use the parser's per-field boost syntax: `^N` after the field name in the
   parser's field-boost map, or by writing the query as `title:foo^3 OR body:foo`.
   With `QueryParser::set_field_boost(title, 3.0)` you get the same effect for every
   query the parser produces.

### Exercise D — Score explanation

Using an in-memory index with at least 5 books, search for a keyword that matches multiple
documents. For each result, print the document title, its score, and the full `explain()`
JSON. Answer: which IDF component is largest and why?

---

## 7. Retrieving Stored Fields

`searcher.doc(doc_address)` fetches a document from the **docstore** — the compressed
row-oriented store holding all `STORED` field values.

```rust
use tantivy::schema::Value;  // brings as_str(), as_f64(), as_u64() etc. into scope

let doc: TantivyDocument = searcher.doc(doc_address)?;

// Single value
if let Some(v) = doc.get_first(title) {
    println!("{}", v.as_str().unwrap());
}

// All values of a multi-valued field
for v in doc.get_all(title) {
    println!("{}", v.as_str().unwrap());
}

// As JSON
println!("{}", doc.to_json(&schema));
```

**Performance warning**: the docstore is block-compressed (LZ4 or Zstd). Fetching one
document requires decompressing the block it belongs to — typically ~16 KB. This is fast for
displaying top-10 results but catastrophically slow if called for every matched document
during scoring. Use `FAST` fields for per-document data access during collection.

As a rule of thumb: **do not hit the docstore more than ~100 times per query**.

### 7.1 Fetching a document by id

Tantivy has **no primary-key get API**. Every document fetch goes through
**search → DocAddress → docstore**. Two cases come up in practice:

**1. By tantivy's internal `DocId`** (segment-local `u32`) — direct, but rarely what
you want:

```rust
let addr = DocAddress { segment_ord: 0, doc_id: 42 };
let doc: TantivyDocument = searcher.doc(addr)?;
```

The internal `DocId` is **not stable**: it shifts when segments merge, and `segment_ord`
shifts when the searcher snapshot changes. Never persist it; treat it as valid only
within the snapshot that produced it (e.g. the `TopDocs` you just collected).

**2. By an application-level id** (the stable id you control). Index it as a `STRING`
field — `STRING` for exact match, `STORED` only if you also want to read it back — and
look it up with a `TermQuery`:

```rust
let id_f = schema.get_field("id").unwrap();
let term = Term::from_field_text(id_f, "user-123");
let query = TermQuery::new(term, IndexRecordOption::Basic);

let hits = searcher.search(&query, &TopDocs::with_limit(1))?;
if let Some((_, addr)) = hits.first() {
    let doc: TantivyDocument = searcher.doc(*addr)?;
    // doc now contains every STORED field
}
```

Practical points:

- **`STORED` alone is not searchable.** Such a field has no inverted-index entry; it
  is pure payload, retrievable only via a `DocAddress` you obtained by some other
  means.
- **One `searcher.doc(addr)` returns every `STORED` field.** You don't fetch them
  individually.
- **Cost.** Docstore reads decompress 16 KB LZ4 blocks with a small LRU cache. A few
  by-id lookups per request is fine; thousands is the wrong tool — use a fast field.
- **Uniqueness is your job.** Tantivy does not enforce id uniqueness. To upsert,
  call `writer.delete_term(Term::from_field_text(id_f, "user-123"))` followed by
  `add_document(...)` and one `commit()`.

### Questions

1. You call `doc.get_first(body)` but `body` was declared as `TEXT` (not `STORED`). What do
   you get?
2. A document was indexed with two `title` values. `doc.get_first(title)` returns which one?
3. Why is accessing a `FAST` field during scoring fine, but accessing the docstore is not?

### Answers

1. `None`. `TEXT` only indexes the field; the original value is not in the docstore.
   You searched it successfully, but you cannot retrieve it. To do both, declare the
   field as `TEXT | STORED`.
2. The **first** value that was added with `doc.add_text(title, ...)`, in insertion
   order. To iterate all values, use `doc.get_all(title)` which returns an iterator
   over every value stored for that field.
3. A fast field is a flat, mmapped column indexed by `DocId`: O(1), no decompression,
   stays in the page cache for hot fields. The docstore is row-oriented and **LZ4-
   compressed in 16 KB blocks**: reading one field forces decompressing the whole
   block containing all stored fields of all docs in that block. Doing that per
   matching document during scoring is many MB of waste per query (CLAUDE.md gives
   the rule of thumb: ≤ ~100 docstore hits per query).

### Exercise E — Selective retrieval

Index 1000 synthetic documents with fields: `id` (STRING), `title` (TEXT | STORED),
`body` (TEXT, **not** STORED), `score_val` (f64, FAST | STORED).

Search for a keyword. For each result:
- Print the title (from docstore).
- Print the `score_val` using a fast field reader instead of the docstore.
  (Hint: `searcher.segment_reader(doc_address.segment_ord).fast_fields()`)
- Confirm that `body` is absent from `doc.to_json()`.

---

## 8. Snippets and Highlighting

A snippet is a short excerpt from a `STORED` text field, with query terms highlighted.

```rust
use tantivy::snippet::SnippetGenerator;

// Build once per query — inspects the query to identify which terms to highlight.
let gen = SnippetGenerator::create(&searcher, &*query, body)?;

for (score, doc_address) in top_docs {
    let doc = searcher.doc::<TantivyDocument>(doc_address)?;
    let snippet = gen.snippet_from_doc(&doc);

    // HTML with <b> tags around matched terms:
    println!("{}", snippet.to_html());

    // Custom rendering:
    let mut out = String::new();
    let mut pos = 0;
    for range in snippet.highlighted() {
        out.push_str(&snippet.fragment()[pos..range.start]);
        out.push_str(">>>");
        out.push_str(&snippet.fragment()[range.clone()]);
        out.push_str("<<<");
        pos = range.end;
    }
    out.push_str(&snippet.fragment()[pos..]);
    println!("{out}");
}
```

The `body` field must be `STORED` for snippet generation — the generator needs the raw text
to extract an excerpt.

### Questions

1. `SnippetGenerator::create` takes the query as `&*query`. What does the `*` do here?
2. The field passed to `SnippetGenerator::create` is `body`. What happens if you pass `title`
   instead, but the matched term only appears in `body`?
3. You want to highlight a field that is `TEXT` but not `STORED`. Is snippet generation
   possible? Why?

### Answers

1. `&*query` reborrows through the smart pointer (e.g. `Box<dyn Query>` or
   `Arc<dyn Query>`): `*query` dereferences to the underlying `dyn Query`, then `&`
   takes a reference, giving the `&dyn Query` that `SnippetGenerator::create`
   expects. The `*` is what unwraps the box; `&` rebuilds a borrow at the right type.
2. The snippet is built from the text of the **field passed to the generator**, not
   from where the match lives. If you pass `title` but the matched terms only appear
   in `body`, the generator scans the title text, finds no matching tokens, and
   returns an empty (or fallback) snippet. Snippet field must match the field whose
   matches you want to highlight.
3. No. The snippet generator needs the original text to render around the matched
   terms — and the original text only lives in the docstore, which requires
   `STORED`. A `TEXT`-only field is searchable but its text is not retrievable, so
   there is nothing to highlight against.

---

## 9. Facets

Facets enable hierarchical category navigation — the kind you see in e-commerce sidebars
("Electronics → Laptops → Gaming"). Each document carries a path, and the `FacetCollector`
counts how many documents belong to each node of the hierarchy.

### Indexing facets

```rust
use tantivy::schema::{FacetOptions, Facet};

let category = sb.add_facet_field("category", FacetOptions::default());

// ...

writer.add_document(doc!(
    name     => "Tiger",
    category => Facet::from("/Felidae/Pantherinae/Panthera"),
))?;
writer.add_document(doc!(
    name     => "Lion",
    category => Facet::from("/Felidae/Pantherinae/Panthera"),
))?;
writer.add_document(doc!(
    name     => "Cat",
    category => Facet::from("/Felidae/Felinae/Felis"),
))?;
```

### Counting facet occurrences

```rust
use tantivy::collector::FacetCollector;
use tantivy::query::AllQuery;

let mut fc = FacetCollector::for_field("category");
fc.add_facet("/Felidae");   // "count one level below /Felidae"
let counts = searcher.search(&AllQuery, &fc)?;

for (facet, count) in counts.get("/Felidae") {
    println!("{facet}: {count}");
}
// /Felidae/Felinae: 1
// /Felidae/Pantherinae: 2
```

### Drilling down — filtering to a facet

```rust
use tantivy::query::TermQuery;
use tantivy::schema::IndexRecordOption;

let facet = Facet::from("/Felidae/Pantherinae");
let term  = Term::from_facet(category, &facet);
let query = TermQuery::new(term, IndexRecordOption::Basic);
let hits  = searcher.search(&query, &TopDocs::with_limit(10).order_by_score())?;
// Returns Tiger, Lion, Jaguar, ...
```

### Questions

1. A document has `category = Facet::from("/Electronics/Laptops/Gaming")`. You count facets
   under `/Electronics`. What sub-facets appear in the result?
2. How is a drill-down query (filter to one facet) different from a facet count?
3. Can a document belong to multiple facets? How?

### Exercise F — Faceted catalogue

Build an index of 12 books with a `genre` facet field:
- 4 books at `/Fiction/SciFi`
- 4 books at `/Fiction/Fantasy`
- 4 books at `/NonFiction/History`

1. Count and print all sub-genres under `/Fiction`.
2. Filter to `/Fiction/SciFi` and print titles.
3. Count all sub-genres under the root `/` and verify the total equals 12.

---

## 10. Aggregations

Aggregations summarise the matched document set — totals, averages, distributions. The API
is compatible with Elasticsearch aggregation JSON.

**All aggregated fields must be `FAST`.**

```rust
use tantivy::aggregation::{agg_req::Aggregations, AggregationCollector,
                            agg_result::AggregationResults};
use tantivy::query::AllQuery;
use serde_json::Value;

let agg_req: Aggregations = serde_json::from_str(r#"
{
  "price_histogram": {
    "histogram": {
      "field": "price",
      "interval": 10.0
    },
    "aggs": {
      "avg_rating": { "avg": { "field": "rating" } }
    }
  }
}
"#)?;

let collector = AggregationCollector::from_aggs(agg_req, Default::default());
let result: AggregationResults = searcher.search(&AllQuery, &collector)?;
let json: Value = serde_json::to_value(result)?;
println!("{}", serde_json::to_string_pretty(&json)?);
```

#### Supported aggregation types

| Type | Category | Description |
|------|----------|-------------|
| `terms` | bucket | Groups by field value (like `GROUP BY`) |
| `range` | bucket | Groups into explicit numeric ranges |
| `histogram` | bucket | Groups into equal-width numeric buckets |
| `date_histogram` | bucket | Groups by time interval (hour, day, month…) |
| `filter` | bucket | A single bucket for documents matching a sub-query |
| `avg` | metric | Average of a numeric field |
| `min` / `max` | metric | Minimum / maximum value |
| `sum` | metric | Sum of a numeric field |
| `count` | metric | Document count (implicit in bucket aggs) |
| `stats` | metric | count, min, max, avg, sum in one request |
| `extended_stats` | metric | All of `stats` plus variance and std deviation |
| `percentiles` | metric | Percentile estimates (uses DDSketch) |
| `top_hits` | metric | Returns the top N documents within a bucket |

Bucket aggregations can be nested: add an `"aggs"` block inside a bucket to run a
sub-aggregation on the documents in each bucket.

### Questions

1. You want the minimum and maximum price per product category in one query. Which two
   aggregation types do you combine?
2. What field option is required for a field to be usable in an aggregation?
3. You run `terms` aggregation on a `TEXT` field tokenised with the default tokeniser. A
   value `"New York"` gets tokenised into `"new"` and `"york"`. What bucket keys will you see?
   How should the field be configured to get `"New York"` as a single bucket?

### Exercise G — Aggregation pipeline

Using the Amazon Video Games index (or any index with a numeric rating and a category field):
1. Compute the average rating per category using `terms` + `avg`.
2. Count the number of reviews per star (1.0 through 5.0) using `histogram` with interval 1.
3. Combine both in a single `searcher.search()` call using the JSON API.

---

## 11. The Full Lifecycle — One Picture

```
 Schema::builder()
       │
       ▼
    Schema  ─────────────────────────────────────────────────────────────────────┐
       │                                                                         │
       ▼                                                                         │ (preserved in
  Index::create_in_dir()                                                         │  meta.json)
       │
       ▼
  IndexWriter ──► add_document(doc) ──► add_document(doc) ──► ...
       │
       │  commit()                              ← atomic: all-or-nothing
       │
       ▼
  Segment files on disk:
    <uuid>.idx       ← posting lists (DocId lists per term)
    <uuid>.term      ← term dictionary (Term → TermInfo)
    <uuid>.pos       ← token positions (for phrase queries)
    <uuid>.fast      ← column store (fast fields)
    <uuid>.fieldnorm ← per-doc field lengths (for BM25)
    <uuid>.store     ← compressed docstore (STORED fields)
    <uuid>.<N>.del   ← alive bitset (marks deleted docs)

  meta.json          ← schema + list of all segments

       │
       ▼
  IndexReader ──► reader.searcher()
                         │
                         ▼  (frozen snapshot)
                    Searcher
                         │
                         ├─ search(&query, &TopDocs::with_limit(10))
                         │        │
                         │        ▼
                         │  Vec<(Score, DocAddress)>
                         │        │
                         │        ▼
                         └─ doc(addr) ──► TantivyDocument
```

---

## 12. Reference Card

```rust
// ── Schema ───────────────────────────────────────────────────────────────────
use tantivy::schema::*;
let mut sb = Schema::builder();
let f = sb.add_text_field("name", TEXT | STORED);
// … other fields …
let schema = sb.build();

// ── Index ────────────────────────────────────────────────────────────────────
let index = Index::create_in_ram(schema);
let index = Index::create_in_dir(path, schema)?;
let index = Index::open_in_dir(path)?;

// ── Write ────────────────────────────────────────────────────────────────────
let mut w = index.writer(50_000_000)?;
w.add_document(doc!(field => value))?;
w.delete_term(Term::from_field_text(field, "id_value"));
w.commit()?;

// ── Read ─────────────────────────────────────────────────────────────────────
let reader = index.reader_builder()
    .reload_policy(ReloadPolicy::OnCommitWithDelay)
    .try_into()?;
reader.reload()?;
let searcher = reader.searcher();
searcher.num_docs();

// ── Build queries ─────────────────────────────────────────────────────────────
use tantivy::query::*;
use std::ops::Bound;

QueryParser::for_index(&index, vec![f]).parse_query("text")?;
TermQuery::new(Term::from_field_text(f, "val"), IndexRecordOption::Basic);
TermQuery::new(Term::from_field_f64(price, 9.99), IndexRecordOption::Basic);
TermSetQuery::new(vec![Term::from_field_text(f, "a"), Term::from_field_text(f, "b")]);
BooleanQuery::new(vec![(Occur::Must, Box::new(q1)), (Occur::Should, Box::new(q2))]);
ConstScoreQuery::new(Box::new(filter_query), 0.0);
DisjunctionMaxQuery::with_tie_breaking_boost(vec![Box::new(q1), Box::new(q2)], 0.1);
RangeQuery::new(Bound::Included(Term::from_field_f64(price, 5.0)),
                Bound::Included(Term::from_field_f64(price, 30.0)));
ExistsQuery::new_exists_query("field_name".to_string());
FuzzyTermQuery::new(Term::from_field_text(f, "typo"), 1, true);
PhraseQuery::new(vec![Term::from_field_text(f, "quick"), Term::from_field_text(f, "fox")]);
PhrasePrefixQuery::new(vec![Term::from_field_text(f, "mach"), Term::from_field_text(f, "learn")]);
RegexQuery::from_pattern("whale.*", f)?;
BoostQuery::new(Box::new(q), 2.0);
// AllQuery, EmptyQuery — no constructor args

// ── Collect ───────────────────────────────────────────────────────────────────
use tantivy::collector::*;
searcher.search(&q, &TopDocs::with_limit(10).order_by_score())?;
searcher.search(&q, &TopDocs::with_limit(10).and_offset(20).order_by_score())?; // page 3
searcher.search(&q, &TopDocs::with_limit(10).order_by_fast_field::<f64>("price", Order::Asc))?;
searcher.search(&q, &TopDocs::with_limit(10).tweak_score(|seg| {
    let col = seg.fast_fields().u64("votes").unwrap().first_or_default_col(0);
    move |doc, score: f32| score * ((2 + col.get_val(doc)) as f32).log2()
}))?;
searcher.search(&q, &Count)?;

// ── Retrieve ──────────────────────────────────────────────────────────────────
use tantivy::schema::Value;
let doc: TantivyDocument = searcher.doc(addr)?;
doc.get_first(field).and_then(|v| v.as_str());
doc.get_all(field);
doc.to_json(&schema);

// ── Score explain ─────────────────────────────────────────────────────────────
query.explain(&searcher, addr)?.to_pretty_json();

// ── Snippet ───────────────────────────────────────────────────────────────────
use tantivy::snippet::SnippetGenerator;
SnippetGenerator::create(&searcher, &*query, body)?
    .snippet_from_doc(&doc)
    .to_html();

// ── Facets ────────────────────────────────────────────────────────────────────
use tantivy::collector::FacetCollector;
let mut fc = FacetCollector::for_field("cat");
fc.add_facet("/root");
let counts = searcher.search(&AllQuery, &fc)?;
counts.get("/root");   // → impl Iterator<Item = (&Facet, u64)>

// ── Aggregations ──────────────────────────────────────────────────────────────
use tantivy::aggregation::{agg_req::Aggregations, AggregationCollector};
let agg_req: Aggregations = serde_json::from_str(r#"{ ... }"#)?;
let res = searcher.search(&q, &AggregationCollector::from_aggs(agg_req, Default::default()))?;
```

---

## 13. Comprehensive Review Questions

Answer all of these before moving to the implementation tutorial.

1. What is the difference between `TEXT` and `STRING`? Give one use case for each.
2. A field is `STORED` but not `TEXT` and not `STRING`. Can you search it? Retrieve it?
3. You call `add_document()` 1000 times, then crash without committing. What is the state of
   the index on restart?
4. Explain snapshot semantics: why does tantivy use them, and name one problem they solve in
   a real application.
5. You need to sort by `price` ascending. What must be true of the `price` field?
6. Explain BM25 TF saturation. Why is matching "whale" 10 times not 10× better than once?
7. You index a document with `title = "The cat sat"`. You search for `"cat sat"` as a phrase
   query. Does it match? What about `"sat cat"`?
8. A `FuzzyTermQuery` with distance 1 — what is the maximum number of single-character
   insertions, deletions, or substitutions allowed?
9. You want to count documents per sub-category and also retrieve the top 3 docs in each
   sub-category. Which aggregations do you combine?
10. What does it mean that segment merges happen "in the background"? Are they visible to
    current searchers?
11. After `delete_term()` + `commit()`, can a searcher acquired *before* the commit still
    see the deleted document?
12. What happens if you call `Index::create_in_dir` on a directory that already has a
    `meta.json`?
