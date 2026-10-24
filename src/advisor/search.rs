//! Service-history search: many phrasings, one ranking.
//!
//! FalconGo's search embeds the user's words once and keeps what lands within a
//! fixed cosine distance. A mechanic's "فرامل" then misses "تيل" and "هوبة",
//! and an English part name misses the Arabic line it was written as. Here
//! Claude first rewrites the question into several variants (Arabic,
//! Egyptian slang, English part names), each variant is searched both ways —
//! pgvector over FalconGo's own embeddings and a normalized keyword match —
//! and every list is merged by reciprocal-rank fusion, so an item that ranks
//! well for several phrasings rises above one lucky hit.
//!
//! Claude never makes vectors: the TEI model FalconGo embedded the invoices
//! with embeds the variants, so query and corpus stay in one space. Without an
//! API key the user's own words are the only variant (today's behaviour).

use std::collections::HashMap;

use chrono::NaiveDate;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::PgPool;

use super::llm::{Completion, LlmProvider, Message, ToolDef, Turn};
use super::upstream;
use super::AdvisorConfig;

/// Reciprocal-rank-fusion constant; 60 is the value from the original paper
/// and the usual default — it keeps one list's top hit from dominating.
const RRF_K: f64 = 60.0;
/// Per variant, per method. Fusion only needs the head of each list.
const PER_LIST: i64 = 40;
/// Beyond this cosine distance a vector hit is noise for this corpus (short
/// Arabic service lines); FalconGo's own cut-off is 0.45.
const MAX_DISTANCE: f64 = 0.6;
pub const MAX_VARIANTS: usize = 8;

#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub car_id: Option<i64>,
    pub plate: Option<String>,
    pub since: Option<NaiveDate>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub item_id: i64,
    pub invoice_id: i64,
    pub car_id: Option<i64>,
    pub plate_number: Option<String>,
    pub date: Option<chrono::DateTime<chrono::Utc>>,
    pub meter_reading: Option<i64>,
    pub service: String,
    pub notes: Option<String>,
    pub score: f64,
    /// Which variants found it, and how ("vector" / "keyword").
    pub matched: Vec<String>,
}

#[derive(sqlx::FromRow)]
struct Row {
    item_id: i64,
    invoice_id: i64,
    car_id: Option<i64>,
    plate_number: Option<String>,
    date: Option<chrono::DateTime<chrono::Utc>>,
    meter_reading: Option<i64>,
    service: String,
    notes: Option<String>,
}

/// The shared projection and filters; `extra` adds columns (the distance).
fn select(extra: &str) -> String {
    format!(
        "SELECT ii.id::bigint AS item_id, si.id::bigint AS invoice_id,
                si.car_id::bigint AS car_id, si.plate_number, si.date,
                si.meter_reading::bigint AS meter_reading, ii.service, ii.notes{extra}
           FROM inspection_items ii
           JOIN service_invoices si ON si.id = ii.service_invoice_id
          WHERE si.deleted_at IS NULL AND ii.deleted_at IS NULL
            AND ($1::bigint IS NULL OR si.car_id = $1)
            AND ($2::text   IS NULL OR si.plate_number = $2)
            AND ($3::date   IS NULL OR si.date >= $3)"
    )
}

/// Arabic letters that are spelled interchangeably in workshop notes fold to
/// one form on both sides of the keyword match (أ/إ/آ→ا, ة→ه, ى→ي).
const FOLD_FROM: &str = "أإآةى";
const FOLD_TO: &str = "اااهي";

fn fold(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| match c {
            'أ' | 'إ' | 'آ' => 'ا',
            'ة' => 'ه',
            'ى' => 'ي',
            c => c,
        })
        .collect()
}

fn vector_literal(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 10);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&x.to_string());
    }
    s.push(']');
    s
}

/// Merge ranked lists. Each list is item ids, best first.
fn fuse(lists: &[(String, Vec<i64>)]) -> Vec<(i64, f64, Vec<String>)> {
    let mut acc: HashMap<i64, (f64, Vec<String>)> = HashMap::new();
    for (label, ids) in lists {
        for (rank, id) in ids.iter().enumerate() {
            let e = acc.entry(*id).or_insert((0.0, Vec::new()));
            e.0 += 1.0 / (RRF_K + rank as f64 + 1.0);
            e.1.push(label.clone());
        }
    }
    let mut out: Vec<_> = acc.into_iter().map(|(id, (s, m))| (id, s, m)).collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// Search every variant both ways and fuse. Returns the top `limit` items.
pub async fn hybrid(
    pool: &PgPool,
    http: &reqwest::Client,
    cfg: &AdvisorConfig,
    variants: &[String],
    filters: &Filters,
    limit: usize,
) -> Result<Vec<Hit>, String> {
    let variants: Vec<String> = variants
        .iter()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .take(MAX_VARIANTS)
        .collect();
    if variants.is_empty() {
        return Ok(Vec::new());
    }
    let mut rows: HashMap<i64, Row> = HashMap::new();
    let mut lists: Vec<(String, Vec<i64>)> = Vec::new();

    // Vectors: one TEI batch for all variants. If the embedder is down the
    // keyword lists still answer.
    match upstream::embed(http, cfg, &variants).await {
        Ok(vectors) => {
            for (variant, vec) in variants.iter().zip(vectors) {
                #[derive(sqlx::FromRow)]
                struct Scored {
                    #[sqlx(flatten)]
                    row: Row,
                    distance: f64,
                }
                let found: Vec<Scored> = sqlx::query_as(&format!(
                    "{} AND ii.embedding IS NOT NULL
                     ORDER BY ii.embedding <=> $4::vector LIMIT {PER_LIST}",
                    select(", (ii.embedding <=> $4::vector)::float8 AS distance")
                ))
                .bind(filters.car_id)
                .bind(&filters.plate)
                .bind(filters.since)
                .bind(vector_literal(&vec))
                .fetch_all(pool)
                .await
                .map_err(|e| format!("vector search: {e}"))?;
                let mut ids = Vec::new();
                for Scored { row, distance } in found {
                    if distance > MAX_DISTANCE {
                        continue;
                    }
                    ids.push(row.item_id);
                    rows.entry(row.item_id).or_insert(row);
                }
                lists.push((format!("{variant} · vector"), ids));
            }
        }
        Err(e) => log::warn!("advisor search: {e}; keyword lists only"),
    }

    for variant in &variants {
        let pattern = format!("%{}%", fold(variant).replace(['%', '_'], ""));
        let found: Vec<Row> = sqlx::query_as::<_, Row>(&format!(
            "{} AND (translate(lower(ii.service), '{FOLD_FROM}', '{FOLD_TO}') LIKE $4
                  OR translate(lower(COALESCE(ii.notes, '')), '{FOLD_FROM}', '{FOLD_TO}') LIKE $4)
             ORDER BY si.date DESC NULLS LAST LIMIT {PER_LIST}",
            select("")
        ))
        .bind(filters.car_id)
        .bind(&filters.plate)
        .bind(filters.since)
        .bind(pattern)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("keyword search: {e}"))?;
        let ids = found.iter().map(|r| r.item_id).collect();
        for row in found {
            rows.entry(row.item_id).or_insert(row);
        }
        lists.push((format!("{variant} · keyword"), ids));
    }

    Ok(fuse(&lists)
        .into_iter()
        .take(limit)
        .filter_map(|(id, score, matched)| {
            let r = rows.remove(&id)?;
            Some(Hit {
                item_id: r.item_id,
                invoice_id: r.invoice_id,
                car_id: r.car_id,
                plate_number: r.plate_number,
                date: r.date,
                meter_reading: r.meter_reading,
                service: r.service,
                notes: r.notes,
                score: (score * 10_000.0).round() / 10_000.0,
                matched,
            })
        })
        .collect())
}

const EXPAND_SYSTEM: &str = "You rewrite a search over the service history of a petroleum-tanker \
trucking fleet in Egypt. The records are short lines written by mechanics, mostly in Egyptian \
Arabic (e.g. 'تغيير تيل فرامل امامي', 'لحام شاسيه', 'تغيير كاوتش دوبل'), sometimes with English \
part names. Produce search variants that a matching record might literally contain: the \
user's own words first, then synonyms, workshop slang, the specific parts the term covers \
(فرامل → تيل، دسك، هوبة، فحمات، طنابير), common misspellings, and the English part name. Short \
phrases (1-3 words), no sentences. Extract a plate number only if the user wrote one.";

fn expand_tool() -> ToolDef {
    ToolDef {
        name: "search_terms",
        description: "The variants to search, best first.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "variants": {
                    "type": "array", "items": { "type": "string" },
                    "minItems": 1, "maxItems": MAX_VARIANTS,
                },
                "plate_number": { "type": "string" }
            },
            "required": ["variants"]
        }),
    }
}

/// Ask Claude for variants. Falls back to the user's words on any failure, so
/// search never gets worse than today's.
pub async fn expand(provider: Option<&dyn LlmProvider>, query: &str) -> (Vec<String>, Option<String>) {
    let fallback = (vec![query.trim().to_string()], None);
    let Some(provider) = provider else {
        return fallback;
    };
    let tools = [expand_tool()];
    let messages = [Message::User(query.to_string())];
    let turn = provider
        .complete(Completion {
            system: EXPAND_SYSTEM,
            messages: &messages,
            tools: &tools,
            max_tokens: 400,
            force_tool: true,
        })
        .await;
    match turn {
        Ok(Turn::Calls(calls)) => {
            let args: &Value = &calls[0].args;
            let mut variants: Vec<String> = args
                .get("variants")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            // The user's own words always search, whatever the model did.
            if !variants.iter().any(|v| v.trim() == query.trim()) {
                variants.insert(0, query.trim().to_string());
            }
            variants.truncate(MAX_VARIANTS);
            let plate = args
                .get("plate_number")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            (variants, plate)
        }
        Ok(Turn::Text(_)) => fallback,
        Err(e) => {
            log::warn!("advisor search expansion failed: {e}");
            fallback
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fusion_rewards_agreement_across_lists() {
        let lists = vec![
            ("a".to_string(), vec![1, 2, 3]),
            ("b".to_string(), vec![3, 4]),
            ("c".to_string(), vec![3, 1]),
        ];
        let fused = fuse(&lists);
        assert_eq!(fused[0].0, 3, "found by all three lists");
        assert_eq!(fused[0].2.len(), 3);
        assert_eq!(fused[1].0, 1);
    }

    #[test]
    fn folding_matches_the_sql_translate() {
        assert_eq!(fold("إطار"), "اطار");
        assert_eq!(fold("هوبة"), "هوبه");
        assert_eq!(fold("Brake"), "brake");
        assert_eq!(FOLD_FROM.chars().count(), FOLD_TO.chars().count());
    }

    #[test]
    fn vectors_render_as_pgvector_literals() {
        assert_eq!(vector_literal(&[0.5, -1.0]), "[0.5,-1]");
    }
}
