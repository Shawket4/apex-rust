//! Notes and tags. Both are purely user-owned, so no override machinery and no
//! reparse interaction -- a reparse must never touch either.

use actix_web::{web, HttpRequest, HttpResponse};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};

use crate::api::{require_read, require_write};
use crate::auth::JwtAuth;
use crate::errors::{AppError, AppResult};








#[derive(Debug, Serialize)]
pub struct Tag {
    pub id: i64,
    pub name: String,
    pub color: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateTag {
    pub name: String,
    pub color: Option<String>,
}

pub(crate) async fn list_tags(pool: web::Data<PgPool>, req: HttpRequest) -> AppResult<HttpResponse> {
    require_read(&req)?;
    let rows = sqlx::query(
        "SELECT id, name, color FROM banksms.tags WHERE deleted_at IS NULL ORDER BY name",
    )
    .fetch_all(pool.get_ref())
    .await?;

    let tags: Vec<Tag> = rows
        .iter()
        .map(|r| Tag {
            id: r.get("id"),
            name: r.get("name"),
            color: r.get("color"),
        })
        .collect();
    Ok(HttpResponse::Ok().json(tags))
}

pub(crate) async fn create_tag(
    pool: web::Data<PgPool>,
    body: web::Json<CreateTag>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    let ctx = require_write(&req)?;
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > 60 {
        return Err(AppError::BadRequest(
            "tag name must be 1-60 characters".into(),
        ));
    }

    // Case-insensitive uniqueness is enforced by a partial index; 23505 maps to
    // 409 automatically via AppError's SQLSTATE classification.
    let row = sqlx::query(
        "INSERT INTO banksms.tags (name, color, created_by) VALUES ($1, $2, $3) \
         RETURNING id, name, color",
    )
    .bind(name)
    .bind(&body.color)
    .bind(ctx.actor())
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Created().json(Tag {
        id: row.get("id"),
        name: row.get("name"),
        color: row.get("color"),
    }))
}

pub(crate) async fn attach_tag(
    pool: web::Data<PgPool>,
    path: web::Path<i64>,
    body: web::Json<serde_json::Value>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    let ctx = require_write(&req)?;
    let transaction_id = path.into_inner();
    let tag_id = body
        .get("tag_id")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| AppError::BadRequest("tag_id is required".into()))?;

    sqlx::query(
        "INSERT INTO banksms.transaction_tags (transaction_id, tag_id, tagged_by) \
         VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(transaction_id)
    .bind(tag_id)
    .bind(ctx.actor())
    .execute(pool.get_ref())
    .await?;

    Ok(HttpResponse::NoContent().finish())
}

pub(crate) async fn detach_tag(
    pool: web::Data<PgPool>,
    path: web::Path<(i64, i64)>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    require_write(&req)?;
    let (transaction_id, tag_id) = path.into_inner();

    sqlx::query(
        "DELETE FROM banksms.transaction_tags WHERE transaction_id = $1 AND tag_id = $2",
    )
    .bind(transaction_id)
    .bind(tag_id)
    .execute(pool.get_ref())
    .await?;

    Ok(HttpResponse::NoContent().finish())
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    let guard = || JwtAuth { required_permission: None };
    // Notes were removed: `transactions.description` already holds free text
    // about a transaction, and two places to write the same thing means neither
    // is trusted and every report has to read both.
    cfg.service(
        web::scope("/api/v1/tags")
            .route("", web::get().to(list_tags).wrap(guard()))
            .route("", web::post().to(create_tag).wrap(guard())),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
}
