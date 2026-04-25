# Public Datasets for Tantivy Experimentation

All datasets are freely available (no payment). Ordered by fit for tantivy practice.

---

## 1. OpenAlex Works Snapshot
**URL:** https://docs.openalex.org/download-all-data/snapshot-data-format  
**S3:** `s3://openalex/` (AWS Open Data, no login)

| | |
|-|-|
| Documents | ~245–260 million works (articles, books, preprints) |
| Format | JSONL.gz, partitioned by entity type |
| Size | ~330 GB compressed / ~1.6 TB uncompressed (sliceable) |

**Schema highlights:**
- Text: `title`, `abstract` (needs reconstruction from inverted index format)
- Numeric: `cited_by_count`, `publication_year`, `referenced_works_count`
- Categorical: `type`, `open_access.oa_status`, `language`, `concepts` (topic tags)
- Dates: `publication_date`

**Best for:** full-text + facet + aggregation on a rich academic corpus. Abstract field requires a one-pass reconstruction step.

---

## 2. Amazon Reviews 2023
**URL:** https://amazon-reviews-2023.github.io  
**Mirror:** https://huggingface.co/datasets/McAuley-Lab/Amazon-Reviews-2023 (no login)

| | |
|-|-|
| Documents | 571 million reviews + ~30 million product metadata records |
| Format | JSONL, split by category (~33 files each for reviews and metadata) |
| Size | ~750 GB total; individual categories 10–50 GB |

**Schema highlights (reviews):**
- Text: `text` (body), `title` (headline)
- Numeric: `rating` (1–5), `helpful_vote`
- Categorical: `category` (33 top-level), `asin`, `verified_purchase`
- Dates: `timestamp`

**Schema highlights (product metadata):**
- Text: `title`, `description`, `features`
- Numeric: `price`, `average_rating`, `rating_number`
- Categorical: `main_category`, `store`, `categories`

**Best for:** aggregations on price/rating, category facets, combined text+numeric queries. Download a single category for a manageable 10M+ slice.

---

## 3. Reddit Comments & Submissions (Pushshift / Academic Torrents)
**URL:** https://academictorrents.com/details/ba051999301b109eab37d16f027b3f49ade2de13  
**Download:** BitTorrent (select individual months/years)

| | |
|-|-|
| Documents | Billions of comments; hundreds of millions of posts (2005–2024) |
| Format | NDJSON.zst (zstd-compressed), one file per month |
| Size | 1–5 GB/month compressed; 10–40 GB uncompressed |

**Schema highlights (comments):**
- Text: `body`
- Numeric: `score`, `ups`, `downs`, `controversiality`
- Categorical: `subreddit`, `author`, `distinguished`
- Dates: `created_utc`

**Schema highlights (posts):**
- Text: `title`, `selftext`, `url`
- Numeric: `score`, `num_comments`, `upvote_ratio`
- Categorical: `subreddit`, `domain`, `link_flair_text`, `is_self`
- Dates: `created_utc`

**Best for:** easiest way to get 10M+ documents in a single month's download. Great for score/date range queries and subreddit facets.

---

## 4. Stack Exchange Data Dump
**URL:** https://archive.org/details/stackexchange_20251231  
**License:** Creative Commons

| | |
|-|-|
| Documents | 70M+ posts (Stack Overflow alone: ~58M); ~100M comments |
| Format | XML inside 7z archives, one archive per site |
| Size | Stack Overflow ~50 GB compressed; full network ~98 GB |

**Schema highlights (Posts):**
- Text: `Body` (HTML), `Title` (questions only)
- Numeric: `Score`, `ViewCount`, `AnswerCount`, `FavoriteCount`
- Categorical: `Tags`, `PostTypeId` (question/answer), `OwnerUserId`
- Dates: `CreationDate`, `LastEditDate`, `ClosedDate`

**Best for:** Q&A text search, tag facets, score-based ranking. Note: avoid the June 2025 dump (data poisoning issue); use December 2025.

---

## 5. Wikipedia English Dump
**URL:** https://dumps.wikimedia.org/enwiki/latest/enwiki-latest-pages-articles.xml.bz2  
**Pre-processed JSONL:** https://huggingface.co/datasets/wikipedia

| | |
|-|-|
| Documents | ~7.2 million English articles |
| Format | XML.bz2 (multistream); JSONL available on Hugging Face |
| Size | ~25 GB compressed / ~105 GB uncompressed |

**Schema highlights:**
- Text: `title`, article body (wikitext markup, needs stripping)
- Categorical: categories (embedded in wikitext), infobox fields
- Dates: `timestamp` of last revision

**Best for:** clean, curated text corpus. Slightly below 10M documents but very high quality. Use the Hugging Face JSONL version to skip markup parsing.

---

## 6. OpenLibrary Data Dumps
**URL:** https://openlibrary.org/data (no login)

| | |
|-|-|
| Documents | 40M+ editions, 35M works, 14M authors |
| Format | TSV with embedded JSON blob (5th column), one file per entity type |
| Size | ~30–35 GB uncompressed for editions |

**Schema highlights:**
- Text: `title`, `subtitle`, `description`, `first_sentence`
- Numeric: `number_of_pages`, `edition_count`
- Categorical: `subjects`, `publishers`, `languages`, `isbn_10`, `isbn_13`
- Dates: `publish_date` (free text, noisy), `last_modified`

**Best for:** book/library search, subject facets. `publish_date` needs normalisation.

---

## 7. arXiv Metadata
**URL:** https://www.kaggle.com/datasets/Cornell-University/arxiv (free Kaggle account)  
**S3:** `s3://arxiv-dataset/arxiv/arxiv/` (free AWS account)

| | |
|-|-|
| Documents | ~2.4 million papers |
| Format | Single JSONL file |
| Size | ~4 GB uncompressed |

**Schema highlights:**
- Text: `title`, `abstract`, `authors`, `journal-ref`
- Categorical: `categories` (e.g. `cs.IR`, `math.CO`), `license`
- Dates: `update_date`

**Best for:** category facets and date-range aggregations. Below 10M docs but pairs well with OpenAlex.

---

## Quick Comparison

| Dataset | Docs | Easiest slice | Rich schema | Format |
|---------|------|--------------|-------------|--------|
| OpenAlex | 245M+ | by entity type | ★★★★★ | JSONL.gz |
| Amazon Reviews | 571M | by category | ★★★★★ | JSONL |
| Reddit | billions | by month | ★★★★☆ | NDJSON.zst |
| Stack Exchange | 70M+ | by site | ★★★★☆ | XML.7z |
| Wikipedia | 7.2M | — | ★★★☆☆ | XML.bz2 / JSONL |
| OpenLibrary | 40M+ | by entity type | ★★★☆☆ | TSV+JSON |
| arXiv | 2.4M | — | ★★★☆☆ | JSONL |

**Recommended starting point:** a single Amazon Reviews 2023 category (e.g. Books or Electronics) gives 10M+ documents with text, numeric (price, rating) and categorical fields — perfect coverage for all tantivy features in one download.
