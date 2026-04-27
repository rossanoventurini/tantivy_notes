# Tantivy Implementation Tutorial — Part 2

A precise, source-grounded guide to tantivy's internals. Every claim here is backed by
actual source code locations so you can read and verify them yourself. Questions and
exercises follow each concept.

---

## 1. The Segment — Unit of Immutability

Every tantivy index is a collection of **segments**. A segment is:

- **Immutable** once written. Nothing inside a segment is ever modified in place.
- **Identified** by a UUID, which becomes the prefix of every file that segment produces.
- **Self-contained**: a segment carries its own term dictionary, posting lists, fast fields,
  docstore, and field norms. It can be searched independently.

When you call `IndexWriter::commit()`, the writer flushes its in-memory buffer into one new
segment per active indexing thread, then atomically rewrites `meta.json` to list the new
segments. Readers pick up the change on their next reload.

Segment merging — combining several small segments into one larger segment — happens in
background threads and produces new immutable segments; the old ones are deleted once no
reader holds a reference to them.

### Segment files on disk

Each segment contributes up to 7 files. The extension encodes the data structure stored:

| Extension | Component | Contains |
|-----------|-----------|---------|
| `.term` | Term dictionary | `Term → TermInfo` mapping. Two-stage structure: FST for Term→TermOrdinal, then SSTable for TermOrdinal→TermInfo. |
| `.idx` | Posting lists | Sorted DocId lists per term, with term frequencies. Delta-encoded, bitpacked in blocks of 128. |
| `.pos` | Positions | Token positions within each document per term. Required for phrase queries. |
| `.fast` | Fast fields | Column-oriented numeric/bool/date/IP storage. Bitpacked. O(1) random access. |
| `.fieldnorm` | Field norms | One byte per document per indexed text field. Encodes field length for BM25. |
| `.store` | Docstore | Compressed, block-oriented row store of all STORED field values (LZ4 or Zstd). |
| `.<opstamp>.del` | Alive bitset | Bitset of alive (non-deleted) DocIds. Rewritten on each commit that changes deletions. The opstamp in the filename allows multiple versions to coexist briefly. |

Source: `src/index/segment_component.rs`, `src/index/index_meta.rs:111–120`.

```
<segment_uuid>.term
<segment_uuid>.idx
<segment_uuid>.pos
<segment_uuid>.fast
<segment_uuid>.fieldnorm
<segment_uuid>.store
<segment_uuid>.<opstamp>.del
```

### meta.json

`meta.json` is the ground truth for which segments exist. It is written atomically (write
to a temp file, then rename) to guarantee consistency. Its structure:

```json
{
  "segments": [
    {
      "segment_id": "abc123...",
      "max_doc": 847291,
      "deletes": { "num_deleted_docs": 12, "opstamp": 7 }
    }
  ],
  "schema": [ ... ],
  "opstamp": 42,
  "payload": null
}
```

`opstamp` is a monotonically increasing operation counter. Every `add_document` and `commit`
increments it. It is used to version deletion bitsets and to detect stale readers.

Source: `src/index/index_meta.rs`.

### Questions

1. You add 1 000 documents in one thread and commit. How many segment files does tantivy
   write at minimum? Name the extensions.
2. What is `meta.json` used for? Why is it written atomically?
3. You create a writer with `writer_with_num_threads(4, 50_000_000)`. You commit. How many
   new segments can be created in that commit?
4. A segment has `max_doc = 10 000` and `num_deleted_docs = 3 000`. What fraction of the
   segment's bytes is wasted? When will those bytes be reclaimed?

### Answers

1. One segment is flushed, so the writer produces one file per active component of that
   segment plus an updated `meta.json` — typically `.term`, `.idx`, `.pos`, `.fast`,
   `.fieldnorm`, `.store` (six segment files), and `meta.json`. No `.del` is written
   because nothing has been deleted on a freshly-built segment, and `.pos` is omitted if
   no field requested positions.
2. `meta.json` is the authoritative list of live segments (UUID, `max_doc`, deletes
   generation, schema). It is written to a temporary file and `rename(2)`d into place so
   readers always see either the old or the new commit — never a partially-updated index.
   This is what makes a commit atomic.
3. Up to 4 — one per indexing thread, but only threads that actually buffered documents
   flush a segment. So the answer is `min(num_threads, num_threads_with_docs)`: 0 if no
   docs were added, up to 4 otherwise.
4. ~30 % of the live data area (3 000 / 10 000 docs) is dead weight, but the cost is
   smaller than that because deletes only mark documents as gone; the inverted index
   entries, fast fields and stored docs for them remain on disk. Those bytes are
   reclaimed only when the segment is **merged** with one or more others by the merge
   policy — the merge rewrites only the live documents into a new segment and the old
   files are unlinked.

### Exercise I-1 — Inspect a real index

Create a small index with the amazon_indexer or any tantivy program. After committing, run:

```bash
ls -lh <index_dir>/
cat <index_dir>/meta.json | python3 -m json.tool
```

Identify each file, match it to the table above. Note the size relationships: which file is
largest? Does the ratio match your expectations given the data?

---

## 2. DocId — The Local Identifier

Within a segment, every document is assigned a `DocId: u32` in the range `[0, max_doc)`.
DocIds are allocated sequentially in the order documents are added.

This compact, dense space is the key to the compression strategies that make tantivy fast:

- **Posting lists** store *deltas* between consecutive DocIds. Small deltas → few bits per
  entry → excellent bitpacking compression.
- **Fast fields** use DocId as a direct array index: `value = array[doc_id]`.
- **The alive bitset** is literally a bitset of size `max_doc`, one bit per DocId.

`DocId` is segment-local. The same document's DocId in segment A is completely unrelated to
a DocId in segment B. The `DocAddress` struct (which searchers return) pairs a `DocId` with
a `segment_ord` to disambiguate.

### Questions

1. Why are DocIds allocated sequentially and why does this matter for compression?
2. A segment has `max_doc = 5000` and the alive bitset shows 4800 live docs. How large is the
   bitset in bytes?
3. You search across 3 segments. The top result is `DocAddress { segment_ord: 1, doc_id: 42 }`.
   What does segment_ord 1 mean?

### Answers

1. Sequential allocation makes the DocId space dense (`[0, max_doc)` with no gaps), and
   density is the precondition for every compression scheme used downstream: posting
   lists are sorted-ascending → small deltas → tight bitpacking; the alive bitset is one
   bit per DocId; fast fields are flat columns indexed by DocId. Random or sparse IDs
   would defeat all three.
2. The bitset is sized by `max_doc`, not by the live count: `ceil(5000 / 8) = 625` bytes.
   Whether 4 800 or 200 docs are alive, the array is the same size — what changes is the
   number of set bits.
3. `segment_ord` is the index of the segment within the `Searcher`'s ordered list of
   `SegmentReader`s. `segment_ord = 1` means "the second segment in this snapshot". It
   is **not** stable across reopens: a commit, a merge, or a new searcher can reshuffle
   the ordering. The persistent identity is the segment's UUID in `meta.json`.

---

## 3. The Inverted Index — Term to DocIds

The inverted index is the core data structure enabling full-text search. It maps each term
to the list of documents containing that term.

```
Term ──► TermOrdinal ──► TermInfo ──► posting list (DocIds + term freqs)
         (via FST)        (via SSTable)  (in .idx file)
                                     └► positions (in .pos file)
```

This two-stage lookup is split across two files:
- `.term` — the term dictionary (FST + SSTable)
- `.idx` — the raw posting list bytes

### 3.1 Term encoding

A `Term` is a sequence of bytes with a type prefix. The byte representation is designed to
preserve the natural ordering of the underlying type so that range queries become byte-range
lookups.

| Field type | Encoding |
|-----------|----------|
| `text` | UTF-8 bytes (one term per token after analysis) |
| `u64` | 8-byte big-endian |
| `i64` | 8-byte big-endian after `(val as u64) ^ 0x8000_0000_0000_0000` (sign-bit flip — equivalent to `val − i64::MIN`, so negatives sort before positives) |
| `f64` | 8-byte big-endian after Lemire's order-preserving mapping: positive values XOR the MSB; negative values bitwise-NOT all bits (`common::f64_to_u64`) |
| `bool` | 8-byte big-endian (`u64::from(self).to_be_bytes()` — `false` = 8 zero bytes, `true` = `0x00…01`) |
| `date` | Internally an `i64` of nanoseconds; **truncated to seconds** at indexing time (`DATE_TIME_PRECISION_INDEXED = Seconds`, `src/schema/date_time_options.rs:9`), then encoded like `i64` |
| `IpAddr` | 16-byte big-endian, IPv4 mapped into IPv6 (`Ipv6Addr::to_u128().to_be_bytes()`) |
| `bytes` / `facet` | raw bytes |

The big-endian encoding means that for two values `a < b`, the byte representation of `a` is
lexicographically less than the byte representation of `b`. A numeric range `[lo, hi]` is
therefore equivalent to a byte-range `[encode(lo), encode(hi)]` in the term dictionary.

**Memory implications of the encoding choice.** A `Term` (`src/schema/term.rs:23–26`) is a
`(Field, Vec<u8>)` pair. The serialized in-memory form is `[field_id: 4 B big-endian]
[type_tag: 1 B][payload]`; the term dictionary is built **per-field per-segment**, so what
ends up as a key inside the FST is just the `[type_tag][payload]` part — the field id
identifies which dictionary, not which key inside it. Numeric, bool and date payloads are
fixed-width (8 B for every `u64`/`i64`/`f64`/`bool`/`date`, 16 B for `IpAddr`), which makes
block layout in the dictionary regular and prefix-sharing trivial across a numeric range.
Text payloads are variable-length UTF-8. The default `"default"` analyser
(`src/tokenizer/tokenizer_manager.rs:60–65`) chains `SimpleTokenizer + RemoveLongFilter::limit(40) + LowerCaser`,
so tokens longer than **40 bytes are *dropped* (filtered out)** before they ever reach the
dictionary — they are not truncated. `STRING` fields use the `"raw"` tokenizer with no
length filter, so STRING values can be arbitrarily long. The size of the term dictionary on
disk is therefore driven by the **cardinality of distinct values per field** (plus the
per-term type-tag), not by the number of documents — a million docs that share a small
vocabulary cost less than a million docs each with a unique high-entropy id.

Source: `src/schema/term.rs`, `src/tokenizer/tokenizer_manager.rs:53–81`,
`columnar/src/column_values/monotonic_mapping.rs:115–187`.

### 3.2 The term dictionary (.term)

The term dictionary is itself a two-layer structure:

**Layer 1 — FST (Finite State Transducer)**

The FST maps each term's byte representation to a `TermOrdinal` (a `u64` ordinal, starting
at 0). The FST is extremely compact — it compresses common prefixes across all terms and is
stored as a single memory-mapped blob. A lookup is O(key length) and requires no heap
allocation.

**Layer 2 — SSTable (TermInfoStore)**

The SSTable maps `TermOrdinal → TermInfo`. `TermInfo` contains:

```rust
pub struct TermInfo {
    pub doc_freq: u32,        // how many documents contain this term
    pub postings_range: Range<usize>,  // byte offset range in the .idx file
    pub positions_range: Range<usize>, // byte offset range in the .pos file
}
```

Source: `src/postings/term_info.rs`.

Together: to look up a term, the FST converts the term bytes to an ordinal in O(|term|), and
the SSTable converts that ordinal to a `TermInfo` in O(1). The `postings_range` is then used
to slice the `.idx` mmap directly.

Source: `src/termdict/mod.rs`, `sstable/`.

### 3.3 Memory and compression of the term dictionary

Both layers are designed to live on disk and be consulted via `mmap`, so the **resident set**
at runtime is just the OS page cache for the pages actually touched — never the full
dictionary. Hot terms stay warm; cold ones cost a page-in but no allocation.

**FST (Layer 1) — compression.** The FST is a *minimised* deterministic finite automaton:
common **prefixes** (like Lucene's prefix trie) and common **suffixes** (which a plain trie
cannot share) are merged into the same state. Output bytes (the term ordinals) are pushed as
far up toward the root as possible so transitions carry partial outputs that sum along the
path — this lets the structure encode a `term → u64` mapping with a state count typically
much smaller than the number of terms. For natural-language text this routinely yields a few
bits per term; for high-cardinality opaque ids (UUIDs, hashes) sharing collapses and the FST
size approaches one node per character, so it ends up close to a sorted concatenation of the
keys plus the ordinal outputs.

**FST — memory implications.** Lookups are O(|term|) byte comparisons against mmap and
allocate nothing on the heap. The build phase, however, requires keys to arrive in sorted
order and keeps a small in-memory buffer of unfinished states — that bounds writer memory
during segment flush regardless of vocabulary size.

**SSTable (Layer 2) — compression.** The `TermInfoStore`
(`src/termdict/fst_termdict/term_info_store.rs`) is laid out as fixed-size blocks of
**`BLOCK_LEN = 256`** consecutive `TermInfo` records (line 12). Each block is described by a
`TermInfoBlockMeta` carrying:

- the file `offset` of the block's bitpacked body,
- a `ref_term_info` (the **first** `TermInfo` of the block, stored uncompressed),
- three independent bit-widths (`doc_freq_nbits`, `postings_offset_nbits`,
  `positions_offset_nbits`) — one per column.

The remaining 255 entries inside the block are encoded as bitpacked **deltas relative to the
block's `ref_term_info`**. Because `postings_range.start` and `positions_range.start` are
monotonically increasing across ordinals, the offsets stay small within a block and pack
cheaply. The `end` of one range is just the `start` of the next, so only one offset per
column per record is actually written (`deserialize_term_info` reads
`posting_start` for the current ordinal and `posting_start` for the next ordinal as the end
— see lines 62–92).

This is why a `TermInfo` lookup is O(1): `block_id = term_ord / 256`; jump into the block
metadata at `block_id × sizeof(TermInfoBlockMeta)`; then decode the bitpacked columns at
`inner_offset = term_ord % 256` (lines 138–153).

**SSTable — memory implications.** The store consists of two `OwnedBytes` slices held by
`TermInfoStore` (lines 96–100): the block-meta array and the bitpacked term-info bodies.
Both are mmap-backed by the `.term` file — there is no separate "index in RAM"; what stays
resident is just the OS page cache for the pages actually touched. Per-block bitpacking
means that a block of 256 cheap terms (small `doc_freq`, small offset deltas) costs only a
few bits per column, while a block containing one very frequent term widens the bit-width
for the entire 256-entry block — locality of cardinality matters slightly for size.

**Putting the two together.** For a text field with strong vocabulary sharing the FST
typically dominates the term dictionary's space; for numeric or id-like fields it shrinks to
near-incompressible and the bitpacked SSTable becomes the larger of the two. The deliberate
split — an FST that is *good at strings* on top of a columnar store that is *good at
integers* — lets each layer use the compression scheme best suited to its payload.

Note on related files (covered later): the `.idx` posting lists use 128-doc bitpacked
blocks (with a VInt-encoded tail), positions in `.pos` are bitpacked similarly, and the
`.store` docstore compresses blocks of stored documents with **LZ4 by default** (Zstd is
the only other option, behind the `zstd-compression` feature; the full set per
`src/store/compressors.rs` is `none | lz4 | zstd`) — see the corresponding sections.

### Questions

1. Why is `u64` stored big-endian rather than little-endian in the term dictionary?
2. You search for `price >= 10.0 AND price <= 50.0`. Describe exactly what tantivy looks up
   in the term dictionary for this range query.
3. A term has `doc_freq = 0`. Is it possible? What would `postings_range` be?
4. The FST maps Term → TermOrdinal. Why is this ordinal needed? Why not map directly to
   TermInfo?

### Answers

1. Big-endian preserves numeric order in lexicographic byte order: for `a < b`, the bytes
   of `a` are also less than the bytes of `b`. Little-endian puts the least-significant
   byte first, so `0x0001 = 256` would compare smaller than `0x0100 = 1` byte-wise. The
   range-query trick "numeric range = byte-range scan of the term dictionary" only works
   under big-endian (with the sign/exponent fix-ups for `i64`/`f64`).
2. Tantivy encodes `10.0` and `50.0` to their 8-byte signed-adjusted big-endian forms,
   then performs a **byte-range scan** of the term dictionary between those two
   encodings. Each term met along the way is mapped through the SSTable to a `TermInfo`,
   and the union of the resulting posting lists yields the matching DocIds.
3. No. A term that ends up in the dictionary was emitted by the writer because at least
   one document contained it, so `doc_freq ≥ 1`. A `doc_freq = 0` would also imply an
   empty `postings_range` (`start == end`), which the writer never produces.
4. The ordinal is a small, contiguous, bitpackable handle that buys columnar storage of
   term statistics. With `Term → TermInfo` directly in the FST, every transition would
   carry a variable-width payload (doc_freq + two `usize` offsets), inflating the FST
   and losing the SSTable's per-block tricks: monotonic offsets stored as deltas,
   `doc_freq` bitpacked to the block's max, single in-RAM block index. The split lets
   the FST do what it is good at (compressing strings) and the SSTable do what it is
   good at (compressing columns of integers).

### Exercise I-2 — Manual term lookup

Write a small Rust program that opens an existing index, acquires a `SegmentReader`, and uses
`inverted_index(field)?` to get the `InvertedIndexReader`. Call `get_term_info(&term)` for
several terms and print the `doc_freq` and byte ranges. Verify that more common terms
(stop words) have higher `doc_freq`.

---

## 4. Posting Lists (.idx) — DocId Lists per Term

A posting list is the list of DocIds of all documents containing a given term, stored sorted
in ascending order. It also stores term frequencies (how many times the term appears per doc).

### 4.1 Block structure

Posting lists are stored in **blocks of 128 DocIds** (`COMPRESSION_BLOCK_SIZE = BitPacker4x::BLOCK_LEN = 128`).

Each full block is:
1. **Delta-encoded**: instead of storing absolute DocIds `[5, 17, 30, ...]`, the list stores
   the *differences* `[5, 12, 13, ...]`. Since DocIds are sorted, deltas are always positive
   and typically small — a document every N positions has delta ≈ N.
2. **Bitpacked**: the deltas are all packed at the minimum number of bits needed to represent
   the largest delta in the block. If the largest delta is 127, only 7 bits per value are
   needed — storing 128 DocIds takes just 128 bytes instead of 512 bytes for 32-bit integers.

The `BitPacker4x` crate uses SSE2 SIMD instructions to compress and decompress 4 DocIds
simultaneously, making block decompression extremely fast.

A **skip list** accompanies the posting list, storing the last DocId in each block and the
byte offset of that block. This allows jumping to the first block that *might* contain a
target DocId without decompressing earlier blocks — critical for `AND` queries where two
posting lists must be intersected.

For **incomplete last blocks** (fewer than 128 docs), variable-length integer (vint) encoding
is used instead. Vint stores each u32 in 1–5 bytes depending on its magnitude.

Source: `src/postings/compression/mod.rs`, `src/postings/skip.rs`.

### 4.2 Term frequencies

Each block of 128 DocIds is followed by a block of 128 term frequencies. These are also
bitpacked (not delta-encoded, since frequencies are not monotone).

If the field is indexed with `IndexRecordOption::Basic` (no frequencies), term frequencies
are not written.

### 4.3 DocSet trait and lazy iteration

When executing a query, tantivy does not load the entire posting list into memory. Instead it
exposes a lazy `DocSet` iterator that decompresses one block at a time as you advance through
it. The decompressed block is a stack-allocated `[u32; 128]` array.

Two posting list iterators can be intersected (`AND`) efficiently because both are sorted:
this is essentially a merge of two sorted sequences, with the skip list used to skip large
gaps quickly.

Source: `src/postings/`, `src/docset.rs`.

### Questions

1. A posting list has 300 DocIds. How many full bitpacked blocks does it have? How are the
   remaining entries encoded?
2. Why are DocIds delta-encoded before bitpacking? What would happen to compression if they
   were stored as absolute values?
3. Two posting lists for terms "war" (5000 docs) and "peace" (3000 docs) are intersected for
   an AND query. Describe how the skip list helps avoid decompressing unnecessary blocks.
4. `IndexRecordOption::Basic` vs `WithFreqs` vs `WithFreqsAndPositions` — what data is stored
   for each option? When is each appropriate?

### Answers

1. `300 / 128 = 2` full bitpacked blocks (256 docs); the remaining `300 - 256 = 44` docs
   are written to the **VInt-encoded tail block** (variable-byte deltas), because the
   bitpacker only operates on full 128-doc groups. The skip list indexes only the
   bitpacked blocks; the tail is always read sequentially.
2. Sorted-ascending DocIds have small consecutive gaps; bitpacking picks one bit-width
   per block of 128, equal to the max delta's bit-length. Typical gaps need 3–8 bits,
   while absolute IDs in a million-doc segment need ~20 bits — a 3–6× expansion if you
   skipped the delta step. Worse, the per-block max would scale with the largest
   absolute id rather than the largest local gap, so even one large doc would widen the
   whole block.
3. The skip list stores, per block, the largest DocId in that block. Intersecting "war"
   ∩ "peace" advances by `seek(target)`: a binary search over the skip list jumps
   straight to the first block whose max ≥ target, and only that block is decompressed.
   Blocks whose max is below the next candidate are skipped entirely — the docs they
   contain are never decoded.
4. - `Basic`: just sorted DocIds. Use for Boolean filters and queries that don't score
     on the term (e.g. occur clauses where the score is irrelevant).
   - `WithFreqs`: DocIds **plus per-doc term frequency**. Required for BM25 / TF-IDF
     scoring on that field.
   - `WithFreqsAndPositions`: also per-doc token positions (in the `.pos` file).
     Required for phrase, proximity, span and "near" queries. Roughly doubles the
     posting-list footprint, so don't enable it on fields that never need phrase
     matching.

### Exercise I-3 — Block counting

Index a large text corpus (e.g. the Amazon reviews dataset). For a common term like "good",
retrieve its `TermInfo`. Compute: how many full 128-doc blocks does its posting list require?
How many bytes does the posting list occupy? What is the bytes-per-DocId ratio?

---

## 5. Token Positions (.pos) — Enabling Phrase Queries

For fields indexed with `TEXT` (which stores positions), the tokeniser records where each
token appears within the document: token 0, 1, 2, ...

Positions are stored in a separate `.pos` file, pointed to by `TermInfo.positions_range`.
Within each document, positions are **delta-encoded** — store the difference between
consecutive positions, not the absolute position.

Example: tokens at positions `[2, 5, 9]` → stored as `[2, 3, 4]`.

When executing a phrase query for `["quick", "brown", "fox"]`, tantivy:
1. Fetches the posting list for each term (DocIds where each term appears).
2. Intersects the three posting lists to find documents containing all three terms.
3. For each candidate document, reads the positions of each term and checks whether
   there exists a consecutive sequence with gaps of exactly 1.

Source: `src/positions/`, `src/query/phrase_query/`.

### Questions

1. A document has the sentence "The fox ate the fox". How are the positions of "fox" stored
   in the position list?
2. A field is indexed with `STRING`. Can you run a phrase query on it? Why?
3. The phrase query `"quick brown fox"` does not match "quick and brown fox" (there is an
   extra word). How does the position comparison detect this mismatch?

### Answers

1. "fox" appears at token positions 1 and 4 (positions are zero-based: `the=0, fox=1,
   ate=2, the=3, fox=4`). The two positions are stored as deltas from 0 — `[1, 3]` —
   bitpacked in the `.pos` file inside the position block belonging to the `<fox, doc>`
   posting entry, with `term_freq = 2` recorded in the `.idx` file.
2. No. `STRING` indexes the whole field as a single, untokenised term, so there are no
   per-token positions to compare and the field carries no `.pos` data. A phrase query
   needs `IndexRecordOption::WithFreqsAndPositions` on a tokenised `TEXT` field.
3. The phrase scorer requires that for each consecutive pair `(t_i, t_{i+1})` of query
   terms there is a position `p` for `t_i` and `p + 1` for `t_{i+1}` in the same doc.
   In "quick and brown fox" the positions are `quick=0, and=1, brown=2, fox=3`. The
   pair `(quick=0, brown)` would need brown at position 1, but brown is at 2 — gap of
   2, not 1 — so the candidate is rejected without ever needing to look at `fox`.

---

## 6. Fast Fields (.fast) — Column Store

Fast fields provide O(1) random access to a single field value for any DocId. They are used
for sorting results, aggregations, and reading values during scoring.

### 6.1 Physical layout

For a numeric fast field (e.g. `u64`), the data is stored as a column:

```
min_value: u64          (stored in the file header)
num_bits:  u8           (bits needed to represent max_value - min_value)

packed data: [(value[0] - min_value), (value[1] - min_value), ...]
             all packed at `num_bits` bits each
```

Fetching value for `DocId d`:

```
bit_offset = num_bits * d
byte_offset = bit_offset / 8
value = min_value + fetch_bits(data[byte_offset..], bit_offset % 8, num_bits)
```

This requires a single memory access (often a cache hit, since access patterns during search
tend to be sequential). The `columnar` crate implements this logic.

Source: `src/fastfield/`, `columnar/`.

### 6.2 Multi-valued fast fields

A fast field can have multiple values per document (e.g. a document with several tags). In
this case the column stores an additional index array giving the start offset of each
document's values.

### 6.3 The alive bitset

The `.del` file stores a `BitSet` (one bit per DocId) marking which documents are alive (not
deleted). Deleted documents are simply those whose bit is 0. All collectors automatically
skip DocIds with a 0 alive bit.

### Questions

1. A `u64` fast field has `min_value = 1000` and `max_value = 65535`. How many bits per doc
   are needed? For 1 million documents, how large is the fast field file in bytes (ignoring
   header)?
2. Why is column-oriented storage faster than row-oriented storage (the docstore) for
   aggregations?
3. The alive bitset is consulted for every matched DocId. Why is this not expensive?

### Answers

1. `range = max - min + 1 = 64 536`; `bits = ceil(log2(64 536)) = 16` bits per doc. For
   1 M docs: `16 × 1 000 000 / 8 = 2 000 000` bytes ≈ **2 MB** (header excluded).
2. An aggregation reads one field from many documents. Column storage keeps that field's
   values contiguous on disk and indexed by `DocId`, so `N` reads stream `N × bits/8`
   bytes — sequential, prefetcher-friendly, often page-cache-resident. The docstore is
   row-oriented and stores all `STORED` fields of a document together, LZ4-compressed
   in 16 KB blocks; reading one field per doc forces decompressing whole blocks of
   unrelated fields, an order-of-magnitude waste in CPU and bytes touched.
3. The bitset is a flat byte array indexed by `DocId`: a single load + bit-test, O(1)
   and branch-friendly. A 1 M-doc bitset is 125 KB, comfortably in L2 cache, so the
   per-doc cost is essentially a memory-resident bit lookup.

### Exercise I-4 — Fast field access

Write a program that opens an index segment and uses:

```rust
let reader = searcher.segment_reader(segment_ord);
let ff_reader = reader.fast_fields();
let rating: Column<f64> = ff_reader.f64("rating")?;
println!("DocId 0 rating: {:?}", rating.first(0));
```

Iterate over 10 DocIds and compare the values to those in the docstore. Verify they match.

---

## 7. Field Norms (.fieldnorm) — BM25 Length Normalisation

The BM25 formula needs `dl` — the number of tokens in a given field for a given document.
Tantivy stores this as one byte per (document, field) pair in the `.fieldnorm` file.

Since one byte can only encode 256 distinct values but documents can have any number of
tokens, the encoding uses a **non-linear lookup table** with 256 entries. The first 41
entries (IDs 0–40) are the exact values 0–40. For larger lengths the table uses an
exponentially growing scale, so large documents with similar lengths map to the same ID.

```
fieldnorm_id 0  →  length 0
fieldnorm_id 1  →  length 1
...
fieldnorm_id 40 →  length 40
fieldnorm_id 41 →  length 42
fieldnorm_id 42 →  length 44
...
fieldnorm_id 255 → length 2_013_265_944
```

Source: `src/fieldnorm/code.rs` — the full `FIELD_NORMS_TABLE` is defined there (256 values).

This quantisation introduces a small scoring imprecision for long documents, but the table
is designed so that the error never exceeds about 6% per step at the exponential part.

The BM25 scorer pre-computes `cached_tf_component(fieldnorm, avg_fieldnorm)` for all 256
possible fieldnorm IDs at query construction time, so during scoring it does a single array
lookup instead of a division.

Source: `src/query/bm25.rs:58–66`.

### Questions

1. A document's `title` field has 41 tokens. What fieldnorm ID is stored? What about 42
   tokens?
2. Why does the fieldnorm table use a non-linear (exponential) scale rather than a linear one?
3. If two documents have 100 000 and 110 000 tokens respectively, will BM25 treat them
   differently or almost the same? Why?

### Answers

1. They land in **different** ids — the table is identity for IDs 0–40 and then jumps to
   even spacings (`FIELD_NORMS_TABLE` in `src/fieldnorm/code.rs`: id 40 = length 40, id 41
   = length 42, id 42 = length 44, …). `fieldnorm_to_id` does a binary search and returns
   `idx − 1` on a miss, so 41 tokens → **id 40** (decoded back to 40 — a one-token
   underestimate), and 42 tokens → **id 41** (decoded back to 42 exactly). The
   ground-truth test in `code.rs:281–286` makes this explicit: `assert_eq!(fieldnorm_to_id(41), 40); assert_eq!(fieldnorm_to_id(42), 41);`.
2. BM25 only ever uses `dl / avgdl`, a ratio. Relative differences matter much more
   between short documents (a 1-token title vs a 5-token title is a 5× ratio change)
   than between long ones (10 000 vs 10 050 is a 0.5 % change with no real effect on
   ranking). The exact-then-exponential scale (identity up to 40, then growing
   spacings — see `code.rs:13–270`) spends precision where it changes scores and saves
   it where it doesn't, all in one byte per doc.
3. Almost the same — but in **adjacent** buckets, not the same one. Looking at
   `FIELD_NORMS_TABLE`, 100 000 tokens → id 115 (decoded as 98 328) and 110 000 tokens
   → id 116 (decoded as 106 520). The decoded `dl` values differ by ~8 k, which on a
   typical `avgdl` produces a tiny BM25 length-normalisation difference — far smaller
   than the ~10 % gap in raw lengths would suggest, because the table is coarse at this
   scale.

---

## 8. The Docstore (.store) — Retrieving Stored Fields

The docstore provides key-value retrieval: given a DocId, return all STORED field values for
that document.

### 8.1 Block structure

Documents are accumulated in an in-memory buffer. When the buffer exceeds **16 KB** (`BLOCK_SIZE = 16_384`), it is compressed (LZ4 by default, Zstd optionally) and flushed as one
block. A skip index maps DocIds to the byte offset of the block containing them, so that
fetching a document requires at most one block decompression.

The reader caches the last **100 recently decompressed blocks** (`DOCSTORE_CACHE_CAPACITY = 100`), so accessing documents within the same block after the first fetch is essentially free.

Source: `src/store/mod.rs:76`, `src/store/reader.rs:25`.

### 8.2 Why the docstore is slow for per-doc access during scoring

Fetching one document requires:
1. Look up the skip index to find the block byte offset — O(log segments).
2. Seek in the mmap to that offset.
3. Decompress the entire block (up to 16 KB, even if only 1 doc is needed).
4. Deserialise the document.

This is fine for 10 results on a SERP. For 100 000 matched documents it is catastrophic.

### Questions

1. You have 1000 documents, each with a `body` field averaging 2 KB. Estimate the number of
   16 KB blocks. How many decompression operations does fetching 10 random documents require?
2. The cache holds 100 blocks. In a search that decompresses 100 unique blocks and then
   repeats the same 10 documents, how many decompression calls happen in total?
3. You want to retrieve a `rating: f64` field for the top 10 000 matched documents during
   aggregation. Should you use the docstore or a fast field? Why?

### Answers

1. Total raw payload ≈ `1000 × 2 KB = 2 MB`. Compressed at LZ4 ratios for natural
   language (~2×), the docstore lands around 1 MB, i.e. ~64 blocks of 16 KB
   (uncompressed), with each block holding ~8 documents. Ten random docs almost
   certainly fall in 10 distinct blocks → **10 decompressions** (8 if two pairs happen
   to share a block).
2. The first 100 unique blocks miss the cache → 100 decompressions to fill it. The 10
   repeat documents reuse blocks already in the cache → 0 additional decompressions.
   **Total: 100.**
3. Fast field, every time. The docstore would force LZ4-decompressing entire 16 KB
   blocks containing all stored fields for every co-located doc — many MB of waste to
   read a few tens of KB of `f64`s. A `f64` fast field gives O(1) random access per
   `DocId` over a contiguous column. The CLAUDE.md rule of thumb (≤ ~100 docstore hits
   per query) exists exactly for this reason.

---

## 9. BM25 Scoring — Full Details

Source: `src/query/bm25.rs`. Constants: `K1 = 1.2`, `B = 0.75`.

### 9.1 IDF

```rust
pub(crate) fn idf(doc_freq: u64, doc_count: u64) -> Score {
    let x = ((doc_count - doc_freq) as f32 + 0.5) / (doc_freq as f32 + 0.5);
    (1.0 + x).ln()
}
```

- `doc_count` = total documents in the index (across all segments, including deleted ones).
- `doc_freq` = number of documents containing the term.
- The `+0.5` in numerator and denominator is a smoothing constant that avoids division by
  zero and reduces the influence of very rare terms slightly.

IDF is computed **once per term per search**, not per document. It is multiplied by
`(1 + K1)` to form the `weight` stored in `Bm25Weight`.

### 9.2 TF normalisation (pre-computed cache)

```rust
fn cached_tf_component(fieldnorm: u32, average_fieldnorm: Score) -> Score {
    K1 * (1.0 - B + B * fieldnorm as f32 / average_fieldnorm)
}
```

This is computed for all 256 fieldnorm IDs at query construction time and stored in a
`[Score; 256]` array (`Bm25Weight.cache`). During scoring, the denominator part is a single
array lookup.

The final per-document score for one term:

```
score(d, t) = weight × (tf / (tf + cache[fieldnorm_id[d]]))

where weight = IDF(t) × (1 + K1)
```

### 9.3 Multi-term queries

For a `BooleanQuery` with multiple `Must` or `Should` terms, the scores from individual
`TermQuery` scorers are **summed**. There is no normalisation by number of terms.

### 9.4 Scoring statistics come from the Searcher, not one segment

IDF requires knowing the global document count and the global term document frequency across
all segments. The `Searcher` collects these statistics by iterating over all its
`SegmentReader`s before constructing the `Bm25Weight`. Each segment scores independently
using the global statistics.

Source: `src/query/bm25.rs:95–145`.

### Questions

1. Term "the" has `doc_freq ≈ doc_count`. Compute the IDF. What score does a document
   matching "the" receive from the IDF component?
2. Term "fulvous" appears in 2 documents out of 1 000 000. Compute the IDF.
3. A document with `title_fieldnorm_id = 5` (5 tokens) matches a term with `tf = 1`. Using
   `K1 = 1.2`, `B = 0.75`, and `average_fieldnorm = 10`, compute the TF factor.
4. You run `explain()` and see that the IDF contribution is 0.0001. What does this tell you
   about the query term?

### Answers

1. `IDF = ln(1 + (N - n + 0.5) / (n + 0.5))`. With `n ≈ N` the argument approaches
   `ln(1 + 0.5/(N + 0.5)) ≈ 0`, so a doc matching "the" gets essentially **0** from
   the IDF factor. The TF factor is also multiplied by IDF, so the term effectively
   contributes nothing to the score — the BM25 way of saying "stop word".
2. `IDF = ln(1 + (1 000 000 - 2 + 0.5) / (2 + 0.5)) = ln(1 + 999 998.5 / 2.5)
   = ln(1 + 399 999.4) ≈ ln(400 000) ≈ 12.9`.
3. Two equivalent ways to write this — pick the one matching how you split the formula:
   - **Tantivy's split form** (used in §9.2 above and in `Bm25Weight::tf_factor`,
     `src/query/bm25.rs:188–193`): `weight = IDF × (K1+1)`,
     `tf_factor = tf / (tf + cache[fieldnorm_id])`, where
     `cache = K1 × (1 − B + B × dl/avgdl) = 1.2 × (0.25 + 0.75 × 0.5) = 0.75`.
     So **`tf_factor = 1 / (1 + 0.75) ≈ 0.571`** — this is what `tf_factor` returns in
     the source.
   - **Classical textbook form**: fold `(K1+1)` into the TF expression, giving
     `tf × (K1+1) / (tf + K1 × (1 − B + B × dl/avgdl)) = 2.2 / 1.75 ≈ 1.257`.
   The final BM25 contribution `weight × tf_factor` is the same in both — just whether
   the `(K1+1)` factor sits in `weight` or in the TF term.
4. The term is essentially everywhere — `n ≈ N`, i.e. it behaves like a stop word
   (think "the", "is"). It contributes almost nothing to ranking; presence vs absence
   barely moves the score.

### Exercise I-5 — Reproduce BM25 by hand

1. Index 5 documents with varying body lengths and a shared term (e.g. "ocean").
2. Search for "ocean".
3. For the top result, call `explain()` and copy the values for `N`, `n`, `freq`, `dl`,
   `avgdl`.
4. Manually compute the score using the formulas above and verify it matches.

---

## 10. The Indexing Pipeline — From add_document() to Disk

### 10.1 The Stacker (in-memory buffer)

When you call `IndexWriter::add_document()`, the document is sent to one of the indexing
threads via a channel. Each thread owns a `Stacker` — a hash map from `Term` to
`(Vec<DocId>, Vec<TermFreq>, Vec<PositionDelta>)`.

The Stacker accumulates documents until either:
- The memory budget is exhausted (based on the `heap_size` passed to `writer()`), or
- `commit()` is called.

At that point the Stacker is serialised to disk as a segment.

Source: `stacker/`, `src/indexer/segment_writer.rs`.

### 10.2 Serialisation to segment files

Serialisation happens in term-sorted order:

1. **Sort terms** — iterate the Stacker's hash map in sorted order (required for FST construction).
2. **Build FST** — feed terms in order to an FST builder; each term is mapped to its ordinal.
3. **Write posting list** — for each term, compress and write its DocId list (delta + bitpack) to `.idx`.
4. **Write positions** — for fields with positions, delta-encode and write to `.pos`.
5. **Write term info** — write `TermInfo` (doc_freq, byte ranges) to the SSTable in `.term`.
6. **Write fast fields** — flush column-oriented numeric data to `.fast`.
7. **Write field norms** — write one byte per (doc, field) to `.fieldnorm`.
8. **Write docstore** — write compressed blocks of stored fields to `.store`.
9. **Update meta.json** — atomically record the new segment in `meta.json`.

Source: `src/indexer/segment_writer.rs`, `src/postings/serializer.rs`.

### 10.3 Multithreaded indexing

The `IndexWriter` spawns N threads (one per CPU core by default). Documents are distributed
round-robin across threads. Each thread independently manages its own Stacker and produces
its own segment on commit. This is why committing with N threads can produce N new segments.

### Questions

1. Why must terms be processed in sorted order during serialisation? What data structure
   requires this?
2. You call `IndexWriter::commit()` without ever calling `add_document()`. How many new
   segments are created?
3. Documents are distributed across threads. If thread 1 indexes documents 1, 3, 5 and
   thread 2 indexes documents 2, 4, 6 — are the DocIds in each segment contiguous or
   interleaved?

### Answers

1. The FST and SSTable are built **streaming**, single-pass, in sorted byte order. The
   FST builder requires keys to arrive in lexicographic order so it can emit minimised
   states as it goes; the SSTable's per-block prefix compression and monotonic offset
   deltas also rely on sorted input. The terms accumulated in memory by `stacker`
   during indexing are sorted at flush time exactly to satisfy this.
2. Zero. With no buffered docs in any thread, no segment is flushed. `meta.json` is
   only rewritten if there is *some* state change to record (e.g. pending deletes); a
   completely empty commit is essentially a no-op.
3. **Contiguous within each segment.** Each indexing thread owns its own
   `SegmentWriter` and assigns DocIds locally in arrival order. Thread 1's segment has
   docs `[0, 1, 2]` for what the user added as "documents 1, 3, 5"; thread 2's segment
   has docs `[0, 1, 2]` for "2, 4, 6". The original add-order numbering is not
   preserved — DocIds are segment-local handles, not global identifiers.

---

## 11. Segment Merges — Background Housekeeping

### 11.1 Why merges happen

After many commits, the index has many small segments. Too many segments hurt search
performance (every query must be evaluated on each segment independently). Merging:
- Reduces the segment count.
- Permanently removes tombstoned (deleted) documents.

### 11.2 LogMergePolicy (the default)

Tantivy groups segments into **logarithmic layers** by document count. Segments in the same
layer are candidates for merging. Key defaults:

| Parameter | Default | Meaning |
|-----------|---------|---------|
| `min_num_segments` | 8 | Minimum number of segments to merge at once |
| `min_layer_size` | 10 000 | All segments smaller than this are in the same layer |
| `max_docs_before_merge` | 10 000 000 | Segments larger than this are never merged |
| `level_log_size` | 0.75 | Layer boundaries grow exponentially with this factor |

Source: `src/indexer/log_merge_policy.rs`.

### 11.3 Merge execution

A merge runs in a background thread. It:
1. Opens segment readers for all source segments.
2. Creates a new segment writer.
3. Iterates all source DocIds in order, skipping deleted ones.
4. Writes all data structures (posting lists, fast fields, docstore, etc.) for the merged
   segment.
5. Atomically updates `meta.json` to replace the source segments with the merged segment.
6. Old segment files are deleted once no reader holds a reference.

The merge produces a new immutable segment; it never modifies the source segments. Readers
that started before the merge completed continue to use the old segments safely.

### Questions

1. You commit 20 times in quick succession with the default policy. When does the first
   merge happen?
2. A merged segment has no `.del` file. Why? (Think about what merging does to deleted docs.)
3. A reader is holding a snapshot that includes old segment A. A merge completes and deletes
   segment A's files. Does the reader crash? Why?

### Answers

1. With the default `LogMergePolicy`, a merge fires when there are enough
   similarly-sized segments in one tier (default `min_num_segments = 8`,
   `DEFAULT_MIN_NUM_SEGMENTS_IN_MERGE` in `src/indexer/log_merge_policy.rs:10`). After
   ~8 single-segment commits the policy picks them up and schedules a merge on the
   indexer's background thread pool — so the *first* merge is queued shortly after the
   8th commit and runs asynchronously.
2. Merging copies only the live documents into a new segment. The deleted ones are
   simply not emitted, so the merged segment starts with `num_deleted_docs = 0` and no
   `.del` file is needed. The "rebirth" is what reclaims the wasted bytes from §1 Q4.
3. No. Tantivy uses reference-counted file handles via the `Directory` abstraction.
   The reader still holds an mmap on segment A's files; even after the merger
   `unlink`s the paths, the underlying inodes stay alive on POSIX filesystems until
   the last open fd / mmap is dropped. The reader continues to see segment A's data
   for the rest of its lifetime; the next `IndexReader::reload()` will pick up the
   merged segment instead.

---

## 12. Range Queries on Numeric Fields — Two Strategies

### 12.1 Strategy 1: Inverted index scan (default)

Because numeric values are encoded as big-endian bytes (preserving ordering), a numeric
range `[lo, hi]` becomes a byte-range `[encode(lo), encode(hi)]` in the term dictionary.

The `InvertedIndexRangeQuery`:
1. Asks the term dictionary for a **range stream** — an iterator over all terms whose byte
   representation falls in the given range.
2. For each matching term, loads its posting list and adds all DocIds to a `BitSet`.
3. Returns the fully materialised BitSet as a DocSet.

This strategy pre-materialises all matching DocIds upfront. It is efficient when the range
matches few terms (e.g. exact value lookup) but expensive when the range is very wide (e.g.
all prices in [0, 10^6]).

Source: `src/query/range_query/range_query.rs:16–30`, `src/query/range_query/range_query.rs:128–213`.

### 12.2 Strategy 2: Fast field scan (lazy)

For IP address fields (and extensible to others), tantivy uses a second strategy: scan the
fast field column directly.

The `FastFieldRangeDocSet`:
1. Opens the fast field column reader.
2. Iterates DocIds from 0 to max_doc sequentially.
3. For each DocId, fetches its value and checks whether it falls in the range.
4. Emits matching DocIds lazily (one at a time without materialising a BitSet).

This is lazy and memory-efficient. It is favoured when the fast field fits in cache or when
the range is wide (many matching documents), because the column-oriented access pattern is
cache-friendly.

Source: `src/query/range_query/fast_field_range_doc_set.rs`,
`src/query/range_query/range_query_fastfield.rs`.

### 12.3 Choosing between strategies

`RangeQuery::new()` inspects the field type. If the field is an IP address field, it uses
Strategy 2. Otherwise it uses Strategy 1 (inverted index scan). In practice, for numeric
fields that have both `INDEXED` and `FAST`, you can also manually construct a
`FastFieldRangeDocSet` to get the lazy strategy.

### Questions

1. You run a range query `price BETWEEN 0 AND 1000000` on a field with 10 000 distinct price
   values. Which strategy does the inverted index scan use? How many posting list lookups
   occur?
2. The fast field scan iterates all DocIds from 0 to max_doc. For a segment with 5 million
   documents, is this slow? Why or why not?
3. What property of the byte encoding makes it possible to turn a numeric range into a
   byte-range scan of the term dictionary?

### Answers

1. The inverted-index strategy walks the term dictionary's byte-range
   `[encode(0), encode(1 000 000)]` and unions the posting list of every distinct value
   met along the way. With 10 000 distinct prices that's **10 000 term-info lookups**
   plus 10 000 posting-list slices to be merged. The cost grows with cardinality, not
   with range width, which is why this strategy gets expensive fast for high-cardinality
   numeric fields and why the fast-field scan exists.
2. Not slow on its own. A 16-bit packed `u64` fast field over 5 M docs is ~10 MB of
   contiguous bytes — sequential reads, prefetcher-friendly, and easily page-cache
   resident. Whether it beats the inverted-index strategy depends on selectivity: dense
   ranges or high-cardinality fields favour the column scan; very selective ranges
   (a handful of distinct values) favour the term-dictionary path.
3. The encoding is **order-preserving**: lexicographic byte order matches numeric
   order (big-endian, with the sign bit flipped for `i64` and the sign + exponent
   adjustment for `f64`). That means the term dictionary, sorted by bytes, is also
   sorted by value, so a numeric `[lo, hi]` becomes a single byte-range
   `[encode(lo), encode(hi)]` scan with no per-key conversion needed.

### Exercise I-6 — Range query performance

Using the Amazon reviews index:
1. Create a range query on `rating` for `[4.0, 5.0]` and on `timestamp_ms` for a 1-year
   window.
2. Use `Count` to count the matching documents.
3. Use `std::time::Instant` to measure how long each query takes.
4. Try adding `FAST` to a field that only had `INDEXED` and re-run. Does it change the speed?

---

## 13. Query Execution — Query → Weight → Scorer

Understanding this chain is essential for implementing custom queries.

```
Query
  └─ create_weight(&searcher, scoring_enabled) → Weight
        └─ (per segment) scorer(segment_reader) → Scorer
                └─ advance() → DocId
                   score() → f32
```

### 13.1 Query

A `Query` is a stateless, shareable description of what to match. It knows nothing about
segments. Its only job is to produce a `Weight`.

```rust
pub trait Query: Debug + Any {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> Result<Box<dyn Weight>>;
    fn explain(&self, searcher: &Searcher, doc: DocAddress) -> Result<Explanation>;
    fn count(&self, searcher: &Searcher) -> Result<usize>;
}
```

### 13.2 Weight

A `Weight` is created once per search. It has access to the `Searcher` and can pre-compute
index-wide statistics (like IDF). It produces a `Scorer` for each segment.

```rust
pub trait Weight: Send + Sync {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> Result<Box<dyn Scorer>>;
    fn explain(&self, reader: &SegmentReader, doc: DocId) -> Result<Explanation>;
}
```

### 13.3 Scorer

A `Scorer` is a `DocSet` — a lazy iterator over matched DocIds in the current segment.
It also provides a `score()` method.

```rust
pub trait Scorer: DocSet {
    fn score(&mut self) -> Score;
}

pub trait DocSet {
    fn advance(&mut self) -> DocId;   // moves to next matching doc
    fn doc(&self) -> DocId;            // current DocId
    fn size_hint(&self) -> u32;
}
```

The `Searcher::search` method iterates over all segment readers, calls `weight.scorer(reader)` for each, then calls `advance()` repeatedly and passes each DocId to the Collector.

Source: `src/query/query.rs`, `src/query/weight.rs`, `src/query/scorer.rs`.

### Questions

1. At which level (Query, Weight, or Scorer) is the IDF computed? Why there?
2. A `BooleanQuery` with `[Must A, Must B]` creates a scorer that intersects two DocSets.
   Describe how the intersection is computed efficiently given that both are sorted.
3. A custom `Scorer` that always returns `score() = 1.0` — what query type would this
   correspond to semantically?

### Answers

1. In the **Weight** (specifically when `Bm25Weight` is constructed). The IDF depends
   only on collection statistics — `N` (doc count) and `n` (`doc_freq`) — and is the
   same for every matching doc. Computing it per-`Scorer` would repeat constant work;
   computing it per-`Weight` lets the per-doc Scorer multiply a precomputed scalar by
   the per-doc TF.
2. Both `DocSet`s yield DocIds in ascending order, so the intersection is a galloping
   merge: hold the smaller scorer's current doc, call `seek(target)` on the other; if
   it lands on the same doc → emit, otherwise advance the smaller one to the larger's
   doc and repeat. The skip list inside each posting list lets `seek` jump entire
   128-doc blocks instead of scanning, so the cost is roughly proportional to the size
   of the smaller posting list, not to the union.
3. A **constant-score** scorer — semantically a Boolean *filter* clause (a `must`
   used only for matching, not ranking) or a `ConstScoreQuery`. Anything that is
   yes/no with no relevance signal: returning a fixed score keeps the scoring
   machinery happy while contributing nothing to ordering.

---

## 14. Capstone — Full Query Trace

Trace a call to `searcher.search(&query, &TopDocs::with_limit(10))` step by step:

1. `Searcher::search` calls `query.weight(EnableScoring::Enabled)` → `BooleanWeight`.
2. `BooleanWeight::new` queries the `Searcher` for global IDF statistics (total docs,
   per-term doc_freq across all segments).
3. For each segment reader in the searcher (there may be many):
   a. `weight.scorer(segment_reader, 1.0)` → `BooleanScorer`.
   b. `BooleanScorer` wraps individual `TermScorer`s for each term, each backed by a block
      postings reader pointing into the `.idx` mmap.
   c. The collector calls `scorer.advance()` → decompresses next block if needed → returns
      next DocId.
   d. If the DocId is alive (checked against the `.del` bitset), `scorer.score()` is called.
   e. Score is computed: `Bm25Weight.weight × (tf / (tf + cache[fieldnorm_id]))` where
      `fieldnorm_id` is looked up in the `.fieldnorm` mmap.
   f. `TopDocs` collector maintains a min-heap of the top-10 (score, DocAddress) pairs.
4. After all segments, `TopDocs` sorts the heap and returns `Vec<(Score, DocAddress)>`.
5. You call `searcher.doc(addr)` → slice the `.store` mmap → decompress the block → deserialise the document.

### Exercise I-7 — Capstone trace

Add `println!` calls (or use `log::debug!`) inside:
- `Bm25Weight::score`
- `BlockPostings::advance`
- `StoreReader::get`

Run a query on a small index and observe the call pattern. Answer:
- How many `advance()` calls are made?
- How many `score()` calls (only live, matched documents)?
- How many `StoreReader::get` calls?

---

## Reference: Key Source Locations

| Topic | File |
|-------|------|
| Segment file extensions | `src/index/index_meta.rs:111–120` |
| meta.json structure | `src/index/index_meta.rs` |
| Term encoding (big-endian) | `src/schema/term.rs` |
| FST term dictionary | `src/termdict/`, `sstable/` |
| TermInfo structure | `src/postings/term_info.rs` |
| Posting block size (128) | `src/postings/compression/mod.rs:3` |
| BitPacker4x SIMD | `bitpacker/` |
| Skip list | `src/postings/skip.rs` |
| Field norm table (256 values) | `src/fieldnorm/code.rs` |
| BM25 formula (K1=1.2, B=0.75) | `src/query/bm25.rs:8–9, 52–66` |
| Docstore block size (16 KB) | `src/store/mod.rs:76` |
| Docstore cache (100 blocks) | `src/store/reader.rs:25` |
| LogMergePolicy defaults | `src/indexer/log_merge_policy.rs:8–15` |
| Range query strategies | `src/query/range_query/` |
| Query/Weight/Scorer traits | `src/query/query.rs`, `weight.rs`, `scorer.rs` |
