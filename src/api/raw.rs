//! Raw message inspection and the human feedback loop.
//!
//! The review queue is ordered by SKELETON FREQUENCY, most-repeated first. That
//! ordering is the whole point: a format seen 40 times without a template is a
//! bank that changed its SMS, and it is costing data on every message. A
//! one-off is almost always a human message that beat the triage gate.

use actix_web::{web, HttpRequest, HttpResponse};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};

use crate::api::{clamp_limit, require_read, require_write};
use crate::auth::JwtAuth;
use crate::errors::{AppError, AppResult};
use crate::parser::worker;

#[derive(Debug, Serialize)]
pub struct RawView {
    pub id: i64,
    pub wa_message_id: String,
    pub sender: Option<String>,
    pub is_from_me: bool,
    pub wa_timestamp: DateTime<Utc>,
    pub body: String,
    pub parse_status: String,
    pub skeleton_hash: Option<String>,
    /// How many messages share this skeleton. The review queue's sort key.
    pub skeleton_count: i64,
    pub triage_score: Option<i16>,
    pub parser_version: Option<i32>,
}

fn row_to_raw(r: &sqlx::postgres::PgRow) -> RawView {
    RawView {
        id: r.get("id"),
        wa_message_id: r.get("wa_message_id"),
        sender: r.get("sender"),
        is_from_me: r.get("is_from_me"),
        wa_timestamp: r.get("wa_timestamp"),
        body: r.get("body"),
        parse_status: r.get("parse_status"),
        skeleton_hash: r.get("skeleton_hash"),
        skeleton_count: r.try_get("skeleton_count").unwrap_or(0),
        triage_score: r.get("triage_score"),
        parser_version: r.get("parser_version"),
    }
}

const SELECT_RAW: &str = r#"
    SELECT r.id, r.wa_message_id, r.sender, r.is_from_me, r.wa_timestamp, r.body,
           r.parse_status::text AS parse_status, r.skeleton_hash,
           r.triage_score, r.parser_version,
           COALESCE(s.cnt, 0) AS skeleton_count
    FROM banksms.raw_messages r
    LEFT JOIN (
        SELECT skeleton_hash, count(*) AS cnt
        FROM banksms.raw_messages
        WHERE skeleton_hash IS NOT NULL
        GROUP BY skeleton_hash
    ) s ON s.skeleton_hash = r.skeleton_hash
"#;

#[derive(Debug, Deserialize)]
pub struct RawQuery {
    pub status: Option<String>,
    pub cursor: Option<i64>,
    pub limit: Option<i64>,
}

async fn list_raw(
    pool: web::Data<PgPool>,
    q: web::Query<RawQuery>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    require_read(&req)?;
    let limit = clamp_limit(q.limit);

    if let Some(s) = &q.status {
        if !matches!(
            s.as_str(),
            "pending" | "parsed" | "partial" | "unmatched" | "ignored" | "error"
        ) {
            return Err(AppError::BadRequest(format!("unknown parse_status {s:?}")));
        }
    }

    let sql = format!(
        "{SELECT_RAW} WHERE ($1::text IS NULL OR r.parse_status::text = $1) \
         AND ($2::bigint IS NULL OR r.id < $2) \
         ORDER BY COALESCE(s.cnt, 0) DESC, r.id DESC LIMIT $3"
    );

    let rows = sqlx::query(&sql)
        .bind(&q.status)
        .bind(q.cursor)
        .bind(limit)
        .fetch_all(pool.get_ref())
        .await?;

    Ok(HttpResponse::Ok().json(rows.iter().map(row_to_raw).collect::<Vec<_>>()))
}

/// The review queue: everything that looked like a bank SMS but did not fully
/// parse. Ordered most-repeated skeleton first.
async fn review_queue(
    pool: web::Data<PgPool>,
    q: web::Query<RawQuery>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    require_read(&req)?;
    let limit = clamp_limit(q.limit);

    let sql = format!(
        "{SELECT_RAW} WHERE r.parse_status IN ('partial', 'unmatched') \
           AND r.reviewed_at IS NULL \
         ORDER BY COALESCE(s.cnt, 0) DESC, r.wa_timestamp DESC LIMIT $1"
    );

    let rows = sqlx::query(&sql).bind(limit).fetch_all(pool.get_ref()).await?;
    Ok(HttpResponse::Ok().json(rows.iter().map(row_to_raw).collect::<Vec<_>>()))
}

/// Audit what the triage gate rejected. Being able to see this is what makes the
/// gate's thresholds tunable rather than a black box.
async fn ignored(
    pool: web::Data<PgPool>,
    q: web::Query<RawQuery>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    require_read(&req)?;
    let limit = clamp_limit(q.limit);

    let sql = format!(
        "{SELECT_RAW} WHERE r.parse_status = 'ignored' \
         AND ($1::bigint IS NULL OR r.id < $1) \
         ORDER BY r.triage_score DESC NULLS LAST, r.id DESC LIMIT $2"
    );

    let rows = sqlx::query(&sql)
        .bind(q.cursor)
        .bind(limit)
        .fetch_all(pool.get_ref())
        .await?;

    Ok(HttpResponse::Ok().json(rows.iter().map(row_to_raw).collect::<Vec<_>>()))
}

#[derive(Debug, Deserialize)]
pub struct Reclassify {
    /// "transaction" -- this really was a bank SMS, put it back in the pipeline.
    /// "noise"       -- this is chatter; remember the skeleton and auto-ignore it.
    pub as_: Option<String>,
    #[serde(rename = "as")]
    pub as_field: Option<String>,
}

/// The feedback loop from §7.3. Misclassification is expected; correcting it must
/// not require a redeploy.
async fn reclassify(
    pool: web::Data<PgPool>,
    path: web::Path<i64>,
    body: web::Json<Reclassify>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    let ctx = require_write(&req)?;
    let id = path.into_inner();

    let kind = body
        .as_field
        .clone()
        .or_else(|| body.as_.clone())
        .ok_or_else(|| AppError::BadRequest("`as` must be \"transaction\" or \"noise\"".into()))?;

    match kind.as_str() {
        "transaction" => {
            worker::reclassify_as_transaction(pool.get_ref(), id, &ctx.actor()).await?;
            // Re-run immediately so the caller sees the result rather than
            // waiting for the next poll cycle.
            worker::run(pool.get_ref(), worker::Scope::Pending, 1000).await?;
            Ok(HttpResponse::Ok().json(serde_json::json!({
                "status": "requeued",
                "raw_message_id": id
            })))
        }
        "noise" => {
            let affected = worker::mark_skeleton_as_noise(pool.get_ref(), id, &ctx.actor()).await?;
            Ok(HttpResponse::Ok().json(serde_json::json!({
                "status": "marked_as_noise",
                "raw_message_id": id,
                "siblings_ignored": affected
            })))
        }
        other => Err(AppError::BadRequest(format!(
            "`as` must be \"transaction\" or \"noise\", got {other:?}"
        ))),
    }
}

async fn reparse_one(
    pool: web::Data<PgPool>,
    path: web::Path<i64>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    require_write(&req)?;
    let id = path.into_inner();

    sqlx::query("UPDATE banksms.raw_messages SET parse_status = 'pending' WHERE id = $1")
        .bind(id)
        .execute(pool.get_ref())
        .await?;

    let run = worker::run(pool.get_ref(), worker::Scope::Pending, 1000).await?;
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "examined": run.examined,
        "parsed": run.parsed,
        "partial": run.partial,
        "unmatched": run.unmatched,
        "ignored": run.ignored,
    })))
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    let guard = || JwtAuth { required_permission: None };
    cfg.service(
        web::scope("/api/v1/raw")
            .route("", web::get().to(list_raw).wrap(guard()))
            .route("/review", web::get().to(review_queue).wrap(guard()))
            .route("/ignored", web::get().to(ignored).wrap(guard()))
            .route("/{id}/reclassify", web::post().to(reclassify).wrap(guard()))
            .route("/{id}/reparse", web::post().to(reparse_one).wrap(guard())),
    );
}
