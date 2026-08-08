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
pub struct Note {
    pub id: i64,
    pub transaction_id: i64,
    pub body: String,
    pub author: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct NoteBody {
    pub body: String,
}

fn validate_note(b: &str) -> AppResult<()> {
    let trimmed = b.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest("note body must not be empty".into()));
    }
    if trimmed.chars().count() > 5000 {
        return Err(AppError::BadRequest("note body is too long".into()));
    }
    Ok(())
}

pub(crate) async fn list_notes(
    pool: web::Data<PgPool>,
    path: web::Path<i64>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    require_read(&req)?;
    let id = path.into_inner();

    let rows = sqlx::query(
        "SELECT id, transaction_id, body, author, created_at, updated_at \
         FROM banksms.notes WHERE transaction_id = $1 AND deleted_at IS NULL \
         ORDER BY created_at DESC",
    )
    .bind(id)
    .fetch_all(pool.get_ref())
    .await?;

    let notes: Vec<Note> = rows
        .iter()
        .map(|r| Note {
            id: r.get("id"),
            transaction_id: r.get("transaction_id"),
            body: r.get("body"),
            author: r.get("author"),
            created_at: r.get("created_at"),
            updated_at: r.get("updated_at"),
        })
        .collect();

    Ok(HttpResponse::Ok().json(notes))
}

pub(crate) async fn create_note(
    pool: web::Data<PgPool>,
    path: web::Path<i64>,
    body: web::Json<NoteBody>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    let ctx = require_write(&req)?;
    let id = path.into_inner();
    validate_note(&body.body)?;

    // The FK would catch this, but a 404 is a clearer answer than a 409 for
    // "that transaction does not exist".
    let exists: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM banksms.transactions WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool.get_ref())
    .await?;
    if exists.is_none() {
        return Err(AppError::NotFound(format!("transaction {id}")));
    }

    let row = sqlx::query(
        "INSERT INTO banksms.notes (transaction_id, body, author) VALUES ($1, $2, $3) \
         RETURNING id, transaction_id, body, author, created_at, updated_at",
    )
    .bind(id)
    .bind(body.body.trim())
    .bind(ctx.actor())
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Created().json(Note {
        id: row.get("id"),
        transaction_id: row.get("transaction_id"),
        body: row.get("body"),
        author: row.get("author"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }))
}

pub(crate) async fn patch_note(
    pool: web::Data<PgPool>,
    path: web::Path<i64>,
    body: web::Json<NoteBody>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    require_write(&req)?;
    let id = path.into_inner();
    validate_note(&body.body)?;

    let result = sqlx::query(
        "UPDATE banksms.notes SET body = $2 WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(body.body.trim())
    .execute(pool.get_ref())
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("note {id}")));
    }
    Ok(HttpResponse::NoContent().finish())
}

pub(crate) async fn delete_note(
    pool: web::Data<PgPool>,
    path: web::Path<i64>,
    req: HttpRequest,
) -> AppResult<HttpResponse> {
    require_write(&req)?;
    let id = path.into_inner();

    let result = sqlx::query(
        "UPDATE banksms.notes SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .execute(pool.get_ref())
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("note {id}")));
    }
    Ok(HttpResponse::NoContent().finish())
}

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
    // Transaction-nested note/tag routes are registered inside the
    // /api/v1/transactions scope (see api::transactions::configure).
    cfg.service(
        web::scope("/api/v1/notes")
            .route("/{id}", web::patch().to(patch_note).wrap(guard()))
            .route("/{id}", web::delete().to(delete_note).wrap(guard())),
    )
    .service(
        web::scope("/api/v1/tags")
            .route("", web::get().to(list_tags).wrap(guard()))
            .route("", web::post().to(create_tag).wrap(guard())),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_blank_and_overlong_notes() {
        assert!(validate_note("").is_err());
        assert!(validate_note("   ").is_err());
        assert!(validate_note(&"x".repeat(5001)).is_err());
        assert!(validate_note("a real note").is_ok());
    }
}
