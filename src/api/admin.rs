//! Admin endpoints: the template registry, skeleton mining, bulk reparse and
//! ingest status. All require permission level 4.

use actix_web::{web, HttpRequest, HttpResponse};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};

use crate::api::require_admin;
use crate::auth::JwtAuth;
use crate::errors::{AppError, AppResult};
use crate::parser::{templates, worker};

#[derive(Debug, Serialize)]
pub struct TemplateView {
    pub id: i64,
    pub name: String,
    pub pattern: String,
    pub date_formats: serde_json::Value,
    pub direction_map: serde_json::Value,
    pub priority: i32,
    pub enabled: bool,
    pub version: i32,
    pub notes: Option<String>,
    pub created_at: DateTime<Utc>,
}

async fn list_templates(pool: web::Data<PgPool>, req: HttpRequest) -> AppResult<HttpResponse> {
    require_admin(&req)?;

    let rows = sqlx::query(
        "SELECT id, name, pattern, date_formats, direction_map, priority, enabled, \
                version, notes, created_at \
         FROM banksms.parse_templates ORDER BY priority, id",
    )
    .fetch_all(pool.get_ref())
    .await?;

    let data: Vec<TemplateView> = rows
        .iter()
        .map(|r| TemplateView {
            id: r.get("id"),
            name: r.get("name"),
            pattern: r.get("pattern"),
            date_formats: r.get("date_formats"),
            direction_map: r.get("direction_map"),
            priority: r.get("priority"),
            enabled: r.get("enabled"),
            version: r.get("version"),
            notes: r.get("notes"),
            created_at: r.get("created_at"),
        })
        .collect();

    Ok(HttpResponse::Ok().json(data))
}

#[derive(Debug, Deserialize)]
pub struct CreateTemplate {
    pub name: String,
    pub pattern: String,
    pub date_formats: Vec<String>,
    pub direction_map: serde_json::Value,
    pub priority: Option<i32>,
    pub notes: Option<String>,
}

/// Create a template.
///
/// The pattern is validated in Rust before insert -- it must compile and must
/// define `amount` and `date` named capture groups. This cannot be a CHECK
/// constraint: Postgres POSIX regex has no `(?P<name>...)` syntax, so a
/// database-side test match would reject every valid pattern.
///
/// Reminder for whoever writes the next one: patterns match NORMALIZED text.
/// Write `الي`, not `إلى`.
async fn create_template(
    pool: web::Data<PgPool>,
    body: web::Json<CreateTemplate>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    let ctx = require_admin(&req)?;
    let t = body.into_inner();

    templates::validate_pattern(&t.pattern)?;

    if t.date_formats.is_empty() {
        return Err(AppError::BadRequest(
            "date_formats must list at least one chrono format -- date formats are \
             per-template on purpose, because a shared parser silently produces wrong dates"
                .into(),
        ));
    }
    if !t.direction_map.is_object() {
        return Err(AppError::BadRequest("direction_map must be an object".into()));
    }

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO banksms.parse_templates \
             (name, pattern, date_formats, direction_map, priority, notes, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
    )
    .bind(&t.name)
    .bind(&t.pattern)
    .bind(serde_json::to_value(&t.date_formats).unwrap())
    .bind(&t.direction_map)
    .bind(t.priority.unwrap_or(100))
    .bind(&t.notes)
    .bind(ctx.actor())
    .fetch_one(pool.get_ref())
    .await?;

    // The compiled set is cached; without this the new template would not take
    // effect until the next restart.
    templates::invalidate_cache();

    Ok(HttpResponse::Created().json(serde_json::json!({ "id": id })))
}

#[derive(Debug, Deserialize)]
pub struct PatchTemplate {
    pub pattern: Option<String>,
    pub date_formats: Option<Vec<String>>,
    pub direction_map: Option<serde_json::Value>,
    pub priority: Option<i32>,
    pub enabled: Option<bool>,
    pub notes: Option<String>,
}

async fn patch_template(
    pool: web::Data<PgPool>,
    path: web::Path<i64>,
    body: web::Json<PatchTemplate>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    require_admin(&req)?;
    let id = path.into_inner();
    let p = body.into_inner();

    if let Some(pattern) = &p.pattern {
        templates::validate_pattern(pattern)?;
    }

    let result = sqlx::query(
        r#"
        UPDATE banksms.parse_templates
        SET pattern       = COALESCE($2, pattern),
            date_formats  = COALESCE($3, date_formats),
            direction_map = COALESCE($4, direction_map),
            priority      = COALESCE($5, priority),
            enabled       = COALESCE($6, enabled),
            notes         = COALESCE($7, notes),
            version       = version + 1
        WHERE id = $1
        "#,
    )
    .bind(id)
    .bind(&p.pattern)
    .bind(p.date_formats.map(|v| serde_json::to_value(v).unwrap()))
    .bind(&p.direction_map)
    .bind(p.priority)
    .bind(p.enabled)
    .bind(&p.notes)
    .execute(pool.get_ref())
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("template {id}")));
    }

    templates::invalidate_cache();
    Ok(HttpResponse::NoContent().finish())
}

#[derive(Debug, Serialize)]
pub struct SkeletonGroup {
    pub skeleton_hash: String,
    pub occurrences: i64,
    pub example_body: String,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub statuses: Vec<String>,
}

/// Recurring formats with no matching template.
///
/// This is the alarm surface: a skeleton recurring N+ times without a template
/// means a bank changed its SMS format and we are losing every one of them. A
/// one-off is almost always a human message and should not page anyone.
async fn skeletons(pool: web::Data<PgPool>, req: HttpRequest) -> AppResult<HttpResponse> {
    require_admin(&req)?;

    let rows = sqlx::query(
        r#"
        SELECT r.skeleton_hash,
               count(*) AS occurrences,
               MIN(r.wa_timestamp) AS first_seen,
               MAX(r.wa_timestamp) AS last_seen,
               (ARRAY_AGG(r.body ORDER BY r.wa_timestamp DESC))[1] AS example_body,
               ARRAY_AGG(DISTINCT r.parse_status::text) AS statuses
        FROM banksms.raw_messages r
        WHERE r.skeleton_hash IS NOT NULL
          AND r.parse_status IN ('partial', 'unmatched')
        GROUP BY r.skeleton_hash
        ORDER BY count(*) DESC, MAX(r.wa_timestamp) DESC
        LIMIT 100
        "#,
    )
    .fetch_all(pool.get_ref())
    .await?;

    let data: Vec<SkeletonGroup> = rows
        .iter()
        .map(|r| SkeletonGroup {
            skeleton_hash: r.get("skeleton_hash"),
            occurrences: r.get("occurrences"),
            example_body: r.get("example_body"),
            first_seen: r.get("first_seen"),
            last_seen: r.get("last_seen"),
            statuses: r.get("statuses"),
        })
        .collect();

    Ok(HttpResponse::Ok().json(data))
}

#[derive(Debug, Deserialize)]
pub struct ReparseRequest {
    /// "reviewable" (default) or "all".
    ///
    /// "reviewable" deliberately excludes rows already `parsed`: adding a
    /// template must not re-derive money that has already reconciled.
    pub scope: Option<String>,
    pub max_rows: Option<i64>,
}

async fn bulk_reparse(
    pool: web::Data<PgPool>,
    body: Option<web::Json<ReparseRequest>>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    require_admin(&req)?;

    let (scope_name, max_rows) = body
        .map(|b| {
            let b = b.into_inner();
            (b.scope.unwrap_or_else(|| "reviewable".into()), b.max_rows.unwrap_or(50_000))
        })
        .unwrap_or_else(|| ("reviewable".into(), 50_000));

    let scope = match scope_name.as_str() {
        "reviewable" => worker::Scope::Reviewable,
        "all" => worker::Scope::All,
        "pending" => worker::Scope::Pending,
        other => {
            return Err(AppError::BadRequest(format!(
                "scope must be reviewable, all or pending (got {other:?})"
            )))
        }
    };

    let run = worker::run(pool.get_ref(), scope, max_rows).await?;

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "scope": scope_name,
        "examined": run.examined,
        "parsed": run.parsed,
        "partial": run.partial,
        "unmatched": run.unmatched,
        "ignored": run.ignored,
        "transactions_created": run.transactions_created,
    })))
}

#[derive(Debug, Serialize)]
pub struct IngestStatus {
    pub chat_jid: Option<String>,
    pub last_wa_timestamp: Option<DateTime<Utc>>,
    pub last_wa_message_id: Option<String>,
    pub last_poll_at: Option<DateTime<Utc>>,
    /// Seconds between the newest ingested message and now.
    pub lag_seconds: Option<i64>,
    pub consecutive_errors: i32,
    pub last_error: Option<String>,
    pub last_error_at: Option<DateTime<Utc>>,
    pub raw_message_count: i64,
    pub counts_by_status: serde_json::Value,
}

async fn ingest_status(pool: web::Data<PgPool>, req: HttpRequest) -> AppResult<HttpResponse> {
    require_admin(&req)?;

    let c = sqlx::query(
        "SELECT chat_jid, last_wa_timestamp, last_wa_message_id, last_poll_at, \
                consecutive_errors, last_error, last_error_at \
         FROM banksms.ingest_cursor WHERE id = 1",
    )
    .fetch_one(pool.get_ref())
    .await?;

    let status_rows = sqlx::query(
        "SELECT parse_status::text AS status, count(*) AS n \
         FROM banksms.raw_messages GROUP BY 1",
    )
    .fetch_all(pool.get_ref())
    .await?;

    let mut counts = serde_json::Map::new();
    let mut total = 0i64;
    for r in &status_rows {
        let n: i64 = r.get("n");
        total += n;
        counts.insert(r.get::<String, _>("status"), serde_json::json!(n));
    }

    let last_wa_timestamp: Option<DateTime<Utc>> = c.get("last_wa_timestamp");

    Ok(HttpResponse::Ok().json(IngestStatus {
        chat_jid: c.get("chat_jid"),
        last_wa_timestamp,
        last_wa_message_id: c.get("last_wa_message_id"),
        last_poll_at: c.get("last_poll_at"),
        lag_seconds: last_wa_timestamp.map(|t| (Utc::now() - t).num_seconds()),
        consecutive_errors: c.get("consecutive_errors"),
        last_error: c.get("last_error"),
        last_error_at: c.get("last_error_at"),
        raw_message_count: total,
        counts_by_status: serde_json::Value::Object(counts),
    }))
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    let guard = || JwtAuth { required_permission: None };
    cfg.service(
        web::scope("/api/v1/admin")
            .route("/templates", web::get().to(list_templates).wrap(guard()))
            .route("/templates", web::post().to(create_template).wrap(guard()))
            .route("/templates/{id}", web::patch().to(patch_template).wrap(guard()))
            .route("/skeletons", web::get().to(skeletons).wrap(guard()))
            .route("/reparse", web::post().to(bulk_reparse).wrap(guard()))
            .route("/ingest-status", web::get().to(ingest_status).wrap(guard())),
    );
}
