use tantivy::schema::*;
use tantivy::Index;

pub struct AmazonSchema {
    pub schema:           Schema,
    pub rating:           Field,
    pub title:            Field,
    pub body:             Field,
    pub asin:             Field,
    pub user_id:          Field,
    pub timestamp_ms:     Field,
    pub helpful_vote:     Field,
    pub verified_purchase: Field,
}

impl AmazonSchema {
    pub fn build() -> Self {
        let mut sb = Schema::builder();

        // Full-text search fields
        let title   = sb.add_text_field("title",   TEXT | STORED);
        let body    = sb.add_text_field("body",    TEXT | STORED);

        // Exact-match / ID fields (no tokenisation)
        let asin    = sb.add_text_field("asin",    STRING | STORED);
        let user_id = sb.add_text_field("user_id", STRING | STORED);

        // Numeric fields (FAST = column store → needed for sorting, range queries, aggregations)
        let rating        = sb.add_f64_field("rating",        FAST | STORED | INDEXED);
        let timestamp_ms  = sb.add_u64_field("timestamp_ms",  FAST | STORED | INDEXED);
        let helpful_vote  = sb.add_u64_field("helpful_vote",  FAST | STORED | INDEXED);

        // Boolean
        let verified_purchase = sb.add_bool_field("verified_purchase", FAST | STORED | INDEXED);

        AmazonSchema {
            schema: sb.build(),
            rating,
            title,
            body,
            asin,
            user_id,
            timestamp_ms,
            helpful_vote,
            verified_purchase,
        }
    }

    pub fn from_index(index: &Index) -> Self {
        let schema = index.schema();
        AmazonSchema {
            rating:           schema.get_field("rating").unwrap(),
            title:            schema.get_field("title").unwrap(),
            body:             schema.get_field("body").unwrap(),
            asin:             schema.get_field("asin").unwrap(),
            user_id:          schema.get_field("user_id").unwrap(),
            timestamp_ms:     schema.get_field("timestamp_ms").unwrap(),
            helpful_vote:     schema.get_field("helpful_vote").unwrap(),
            verified_purchase: schema.get_field("verified_purchase").unwrap(),
            schema,
        }
    }
}
