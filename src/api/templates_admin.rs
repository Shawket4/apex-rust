//! Template management over HTTP (curl is the client — there is no UI screen).
//!
//! A new bank format should need neither a deploy nor SSH: POST the row, and
//! the reparse sweep applies it to the stored backlog. Every write validates
//! the pattern against its own sample end to end (compiles, has amount+date
//! groups, matches post-normalization, direction resolves) — the checks that
//! make "pattern can never match" and "NULL direction" impossible to ship.

use actix_web::{web, HttpResponse};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use std::collections::HashMap;

use crate::errors::{AppError, AppResult};
use crate::parser::templates::validate;

#[derive(Debug, Serialize)]
pub struct TemplateView {
    pub id: i64,
    pub name: String,
    pub pattern: String,
    pub date_formats: serde_json::Value,
    pub direction_map: serde_json::Value,
    pub sample: String,
    pub priority: i32,
    pub enabled: bool,
    pub notes: Option<String>,
}

fn row_to_view(r: &sqlx::postgres::PgRow) -> TemplateView {
    TemplateView {
        id: r.get("id"),
        name: r.get("name"),
        pattern: r.get("pattern"),
        date_formats: r.get("date_formats"),
        direction_map: r.get("direction_map"),
        sample: r.get("sample"),
        priority: r.get("priority"),
        enabled: r.get("enabled"),
        notes: r.get("notes"),
    }
}

const COLUMNS: &str =
    "id, name, pattern, date_formats, direction_map, sample, priority, enabled, notes";

pub async fn list(pool: web::Data<PgPool>) -> AppResult<HttpResponse> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM banksms.parse_templates ORDER BY priority, id"
    ))
    .fetch_all(pool.get_ref())
    .await?;
    let out: Vec<TemplateView> = rows.iter().map(row_to_view).collect();
    Ok(HttpResponse::Ok().json(out))
}

#[derive(Debug, Deserialize)]
pub struct CreateTemplate {
    pub name: String,
    pub pattern: String,
    pub date_formats: Vec<String>,
    #[serde(default)]
    pub direction_map: HashMap<String, String>,
    pub sample: String,
    pub priority: i32,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub notes: Option<String>,
}

fn default_true() -> bool {
    true
}

pub async fn create(
    pool: web::Data<PgPool>,
    body: web::Json<CreateTemplate>,
) -> AppResult<HttpResponse> {
    let b = body.into_inner();
    validate(&b.pattern, &b.date_formats, &b.direction_map, &b.sample)?;

    let row = sqlx::query(&format!(
        "INSERT INTO banksms.parse_templates
             (name, pattern, date_formats, direction_map, sample, priority, enabled, notes)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING {COLUMNS}"
    ))
    .bind(&b.name)
    .bind(&b.pattern)
    .bind(serde_json::to_value(&b.date_formats).unwrap())
    .bind(serde_json::to_value(&b.direction_map).unwrap())
    .bind(&b.sample)
    .bind(b.priority)
    .bind(b.enabled)
    .bind(&b.notes)
    .fetch_one(pool.get_ref())
    .await?;

    // Apply the new template to the stored backlog. Fire-and-forget: the
    // insert already succeeded, and the sweep is idempotent.
    let sweep_pool = pool.get_ref().clone();
    tokio::spawn(async move {
        match crate::ingest::poller::reparse_sweep(&sweep_pool).await {
            Ok((changed, created)) => log::info!(
                "reparse after template create: {changed} status change(s), {created} transaction(s)"
            ),
            Err(e) => log::error!("reparse after template create failed: {e}"),
        }
    });

    Ok(HttpResponse::Created().json(row_to_view(&row)))
}

#[derive(Debug, Deserialize)]
pub struct PatchTemplate {
    pub pattern: Option<String>,
    pub date_formats: Option<Vec<String>>,
    pub direction_map: Option<HashMap<String, String>>,
    pub sample: Option<String>,
    pub priority: Option<i32>,
    pub enabled: Option<bool>,
    pub notes: Option<String>,
}

pub async fn patch(
    pool: web::Data<PgPool>,
    path: web::Path<i64>,
    body: web::Json<PatchTemplate>,
) -> AppResult<HttpResponse> {
    let id = path.into_inner();
    let b = body.into_inner();

    let row = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM banksms.parse_templates WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound(format!("template {id}")))?;

    let current = row_to_view(&row);
    let pattern = b.pattern.unwrap_or(current.pattern);
    let date_formats: Vec<String> = match b.date_formats {
        Some(v) => v,
        None => serde_json::from_value(current.date_formats).unwrap_or_default(),
    };
    let direction_map: HashMap<String, String> = match b.direction_map {
        Some(v) => v,
        None => serde_json::from_value(current.direction_map).unwrap_or_default(),
    };
    let sample = b.sample.unwrap_or(current.sample);
    let priority = b.priority.unwrap_or(current.priority);
    let enabled = b.enabled.unwrap_or(current.enabled);
    let notes = b.notes.or(current.notes);

    // The merged row must validate even when only priority changed — a
    // template that no longer passes its sample must not survive an edit.
    validate(&pattern, &date_formats, &direction_map, &sample)?;

    let row = sqlx::query(&format!(
        "UPDATE banksms.parse_templates SET
             pattern = $1, date_formats = $2, direction_map = $3, sample = $4,
             priority = $5, enabled = $6, notes = $7, updated_at = now()
         WHERE id = $8
         RETURNING {COLUMNS}"
    ))
    .bind(&pattern)
    .bind(serde_json::to_value(&date_formats).unwrap())
    .bind(serde_json::to_value(&direction_map).unwrap())
    .bind(&sample)
    .bind(priority)
    .bind(enabled)
    .bind(&notes)
    .bind(id)
    .fetch_one(pool.get_ref())
    .await?;

    let sweep_pool = pool.get_ref().clone();
    tokio::spawn(async move {
        match crate::ingest::poller::reparse_sweep(&sweep_pool).await {
            Ok((changed, created)) => log::info!(
                "reparse after template edit: {changed} status change(s), {created} transaction(s)"
            ),
            Err(e) => log::error!("reparse after template edit failed: {e}"),
        }
    });

    Ok(HttpResponse::Ok().json(row_to_view(&row)))
}
