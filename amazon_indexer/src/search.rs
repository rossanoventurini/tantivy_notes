/// Interactive demo of tantivy queries over the Amazon Reviews index.
///
/// Usage:
///   cargo run --bin search -- <index_dir>

mod schema;

use schema::AmazonSchema;

use std::ops::Bound;
use std::path::Path;

use tantivy::collector::{Count, TopDocs};
use tantivy::query::{BooleanQuery, FuzzyTermQuery, Occur, QueryParser, RangeQuery, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{DocAddress, Index, Order, ReloadPolicy, Searcher, TantivyDocument, Term};

fn main() -> tantivy::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("Usage: search <index_dir>");
        std::process::exit(1);
    }

    let index = Index::open_in_dir(Path::new(&args[1]))?;
    let s     = AmazonSchema::from_index(&index);

    let reader   = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()?;
    let searcher = reader.searcher();

    println!("Index: {} docs, {} segments\n",
        searcher.num_docs(),
        searcher.segment_readers().len()
    );

    // ── Demo 1: Free-text search (BM25) ──────────────────────────────────────
    println!("=== Demo 1: free-text \"graphics card driver\" ===");
    {
        let qp    = QueryParser::for_index(&index, vec![s.title, s.body]);
        let query = qp.parse_query("graphics card driver")?;
        let hits: Vec<(f32, DocAddress)> =
            searcher.search(&query, &TopDocs::with_limit(5).order_by_score())?;
        print_bm25_hits(&searcher, &s, &hits);
    }

    // ── Demo 2: Boolean query (text AND rating range) ─────────────────────────
    println!("\n=== Demo 2: \"controller\" in title AND rating ≥ 4.0, top 5 by rating ===");
    {
        let term_q = TermQuery::new(
            Term::from_field_text(s.title, "controller"),
            IndexRecordOption::Basic,
        );
        let rating_q = RangeQuery::new(
            Bound::Included(Term::from_field_f64(s.rating, 4.0)),
            Bound::Unbounded,
        );
        let bool_q = BooleanQuery::new(vec![
            (Occur::Must, Box::new(term_q)   as Box<dyn tantivy::query::Query>),
            (Occur::Must, Box::new(rating_q) as Box<dyn tantivy::query::Query>),
        ]);
        let hits: Vec<(Option<f64>, DocAddress)> = searcher.search(
            &bool_q,
            &TopDocs::with_limit(5).order_by_fast_field::<f64>("rating", Order::Desc),
        )?;
        print_field_hits(&searcher, &s, &hits);
    }

    // ── Demo 3: Verified purchases, most helpful first ────────────────────────
    println!("\n=== Demo 3: verified purchases, sorted by helpful_vote desc ===");
    {
        let query = TermQuery::new(
            Term::from_field_bool(s.verified_purchase, true),
            IndexRecordOption::Basic,
        );
        let hits: Vec<(Option<u64>, DocAddress)> = searcher.search(
            &query,
            &TopDocs::with_limit(5).order_by_fast_field::<u64>("helpful_vote", Order::Desc),
        )?;
        print_field_hits(&searcher, &s, &hits);
    }

    // ── Demo 4: Timestamp range — reviews from year 2020 ─────────────────────
    println!("\n=== Demo 4: reviews from 2020 (timestamp range), top 5 by rating ===");
    {
        let range_q = RangeQuery::new(
            Bound::Included(Term::from_field_u64(s.timestamp_ms, 1_577_836_800_000u64)), // 2020-01-01
            Bound::Excluded(Term::from_field_u64(s.timestamp_ms, 1_609_459_200_000u64)), // 2021-01-01
        );
        let hits: Vec<(Option<f64>, DocAddress)> = searcher.search(
            &range_q,
            &TopDocs::with_limit(5).order_by_fast_field::<f64>("rating", Order::Desc),
        )?;
        print_field_hits(&searcher, &s, &hits);
    }

    // ── Demo 5: Fuzzy search (typo tolerance) ────────────────────────────────
    println!("\n=== Demo 5: fuzzy search — \"playstaion\" (1-edit distance) ===");
    {
        let term  = Term::from_field_text(s.body, "playstaion");
        let query = FuzzyTermQuery::new(term, 1, true);
        let hits: Vec<(f32, DocAddress)> =
            searcher.search(&query, &TopDocs::with_limit(5).order_by_score())?;
        print_bm25_hits(&searcher, &s, &hits);
    }

    // ── Demo 6: Count only ────────────────────────────────────────────────────
    println!("\n=== Demo 6: count of 1-star reviews ===");
    {
        let query = RangeQuery::new(
            Bound::Included(Term::from_field_f64(s.rating, 1.0)),
            Bound::Included(Term::from_field_f64(s.rating, 1.0)),
        );
        let count = searcher.search(&query, &Count)?;
        println!("  1-star reviews: {count}");
    }

    // ── Demo 7: BM25 explanation ──────────────────────────────────────────────
    println!("\n=== Demo 7: BM25 explanation for top result of \"multiplayer lag\" ===");
    {
        let qp    = QueryParser::for_index(&index, vec![s.title, s.body]);
        let query = qp.parse_query("multiplayer lag")?;
        let hits: Vec<(f32, DocAddress)> =
            searcher.search(&query, &TopDocs::with_limit(1).order_by_score())?;
        if let Some((_score, addr)) = hits.into_iter().next() {
            println!("{}", query.explain(&searcher, addr)?.to_pretty_json());
        } else {
            println!("  No results.");
        }
    }

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn print_doc(searcher: &Searcher, s: &AmazonSchema, addr: DocAddress, score_str: &str) {
    let doc: TantivyDocument = searcher.doc(addr).unwrap();
    let title   = doc.get_first(s.title).and_then(|v| v.as_str()).unwrap_or("—");
    let rating  = doc.get_first(s.rating).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let helpful = doc.get_first(s.helpful_vote).and_then(|v| v.as_u64()).unwrap_or(0);
    println!("  [{score_str}] ★{rating:.1}  👍{helpful}  {}", &title[..title.len().min(80)]);
}

fn print_bm25_hits(searcher: &Searcher, s: &AmazonSchema, hits: &[(f32, DocAddress)]) {
    if hits.is_empty() { println!("  (no results)"); return; }
    for (score, addr) in hits {
        print_doc(searcher, s, *addr, &format!("{score:.3}"));
    }
}

fn print_field_hits<T: std::fmt::Debug>(
    searcher: &Searcher,
    s: &AmazonSchema,
    hits: &[(Option<T>, DocAddress)],
) {
    if hits.is_empty() { println!("  (no results)"); return; }
    for (score, addr) in hits {
        let score_str = score.as_ref().map(|v| format!("{v:?}")).unwrap_or_else(|| "None".into());
        print_doc(searcher, s, *addr, &score_str);
    }
}
