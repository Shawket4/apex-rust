//! Splitting one bank transaction into parts.
//!
//! The parent keeps the message link and the full amount and leaves the
//! ledger (`split_at` set); children are ordinary rows that carry the
//! categories and people, each registering its own loan when its category
//! says so. The one hard rule: **children sum exactly to the parent** —
//! validated here inside the same database transaction that writes them, and
//! re-validated on every edit of the set. Child money fields cannot be edited
//! individually (that would break the sum silently); the set is edited as a
//! whole through PUT, or dissolved through unsplit.

use actix_web::{web, HttpRequest, HttpResponse};
use rust_decimal::Decimal;
use serde::Deserialize;
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::str::FromStr;

use super::registration::{self, Registrable};
use crate::errors::{AppError, AppResult};

#[derive(Debug, Deserialize)]
pub struct SplitPart {
    pub amount: String,
    pub category: Option<String>,
    pub description: Option<String>,
    pub driver_id: Option<i64>,
    pub employee_id: Option<i64>,
    pub paid_by: Option<String>,
    pub car_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct SplitBody {
    pub parts: Vec<SplitPart>,
}

struct Parent {
    id: i64,
    version: i32,
    direction: String,
    amount: Decimal,
    currency: String,
    occurred_at: chrono::DateTime<chrono::Utc>,
    account: Option<String>,
    counterparty: Option<String>,
    reference: Option<String>,
    split_at: Option<chrono::DateTime<chrono::Utc>>,
    parent_id: Option<i64>,
}

async fn load_parent(tx: &mut Transaction<'_, Postgres>, id: i64) -> AppResult<Parent> {
    let r = sqlx::query(
        "SELECT id, version, direction, amount, currency, occurred_at, account,
                counterparty, reference, split_at, parent_id
         FROM banksms.transactions
         WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| AppError::NotFound(format!("transaction {id}")))?;

    Ok(Parent {
        id: r.get("id"),
        version: r.get("version"),
        direction: r.get("direction"),
        amount: r.get("amount"),
        currency: r.get("currency"),
        occurred_at: r.get("occurred_at"),
        account: r.get("account"),
        counterparty: r.get("counterparty"),
        reference: r.get("reference"),
        split_at: r.get("split_at"),
        parent_id: r.get("parent_id"),
    })
}

fn check_if_match(req: &HttpRequest, actual: i32) -> AppResult<()> {
    let header = req
        .headers()
        .get("If-Match")
        .ok_or(AppError::PreconditionRequired)?
        .to_str()
        .map_err(|_| AppError::BadRequest("unreadable If-Match header".into()))?
        .trim()
        .trim_matches('"');
    let expected: i32 = header
        .parse()
        .map_err(|_| AppError::BadRequest("If-Match must be the row version".into()))?;
    if expected != actual {
        return Err(AppError::VersionConflict { expected, actual });
    }
    Ok(())
}

/// Validate parts and insert them as children. Returns the created ids.
async fn insert_parts(
    tx: &mut Transaction<'_, Postgres>,
    parent: &Parent,
    parts: &[SplitPart],
    actor: &str,
) -> AppResult<Vec<i64>> {
    if parts.len() < 2 {
        return Err(AppError::BadRequest(
            "a split needs at least two parts — otherwise just edit the row".into(),
        ));
    }

    let mut sum = Decimal::ZERO;
    for p in parts {
        let amount = Decimal::from_str(p.amount.trim())
            .map_err(|_| AppError::BadRequest(format!("'{}' is not a valid amount", p.amount)))?;
        if amount <= Decimal::ZERO {
            return Err(AppError::BadRequest("every part must be positive".into()));
        }
        if p.driver_id.is_some() && p.employee_id.is_some() {
            return Err(AppError::BadRequest(
                "a part belongs to a driver or an employee, not both".into(),
            ));
        }
        sum += amount;
    }
    if sum != parent.amount {
        return Err(AppError::BadRequest(format!(
            "parts sum to {sum} but the transfer is {} — the split must account for \
             every pound, exactly",
            parent.amount
        )));
    }

    let mut ids = Vec::with_capacity(parts.len());
    for p in parts {
        let amount = Decimal::from_str(p.amount.trim()).unwrap();
        let row = sqlx::query(
            r#"
            INSERT INTO banksms.transactions
                (source, parent_id, direction, amount, currency, occurred_at,
                 account, counterparty, reference, category, description,
                 driver_id, employee_id, paid_by, car_id, created_by)
            VALUES ('split', $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                    $13, $14, $15)
            RETURNING id
            "#,
        )
        .bind(parent.id)
        .bind(&parent.direction)
        .bind(amount)
        .bind(&parent.currency)
        .bind(parent.occurred_at)
        .bind(&parent.account)
        .bind(&parent.counterparty)
        .bind(&parent.reference)
        .bind(&p.category)
        .bind(&p.description)
        .bind(p.driver_id)
        .bind(p.employee_id)
        .bind(&p.paid_by)
        .bind(p.car_id)
        .bind(actor)
        .fetch_one(&mut **tx)
        .await?;
        let id: i64 = row.get("id");

        let reg = Registrable {
            id,
            amount,
            occurred_at: parent.occurred_at,
            description: p.description.clone(),
            category: p.category.clone(),
            driver_id: p.driver_id,
            employee_id: p.employee_id,
            loan_id: None,
        };
        let rule = registration::load_rule(tx, reg.category.as_deref()).await?;
        registration::validate(rule.as_ref(), &reg)?;
        registration::reconcile(tx, &reg).await?;
        ids.push(id);
    }
    Ok(ids)
}

/// Soft-delete every live child, unregistering unpaid loans. A settled loan on
/// any child refuses the whole operation — same rule as everywhere else.
async fn remove_children(tx: &mut Transaction<'_, Postgres>, parent_id: i64) -> AppResult<()> {
    let children = sqlx::query(
        "SELECT id, amount, occurred_at, description, category, driver_id,
                employee_id, loan_id
         FROM banksms.transactions
         WHERE parent_id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(parent_id)
    .fetch_all(&mut **tx)
    .await?;

    for c in &children {
        let reg = Registrable {
            id: c.get("id"),
            amount: c.get("amount"),
            occurred_at: c.get("occurred_at"),
            description: c.get("description"),
            category: c.get("category"),
            driver_id: c.get("driver_id"),
            employee_id: c.get("employee_id"),
            loan_id: c.get("loan_id"),
        };
        registration::unregister_for_delete(tx, &reg).await?;
        sqlx::query(
            "UPDATE banksms.transactions
             SET deleted_at = now(), version = version + 1, updated_at = now()
             WHERE id = $1",
        )
        .bind(reg.id)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// POST /transactions/{id}/split — split an unsplit cash-out row.
pub async fn split(
    pool: web::Data<PgPool>,
    req: HttpRequest,
    path: web::Path<i64>,
    body: web::Json<SplitBody>,
) -> AppResult<HttpResponse> {
    let ctx = super::ctx(&req)?;
    let id = path.into_inner();
    let mut tx = pool.begin().await?;

    let parent = load_parent(&mut tx, id).await?;
    check_if_match(&req, parent.version)?;
    if parent.parent_id.is_some() {
        return Err(AppError::BadRequest("a split part cannot be split again".into()));
    }
    if parent.split_at.is_some() {
        return Err(AppError::Conflict(
            "already split — edit the existing split instead".into(),
        ));
    }
    if parent.direction != "out" {
        return Err(AppError::BadRequest(
            "only cash-out transactions can be split".into(),
        ));
    }
    let has_loan: Option<i64> =
        sqlx::query_scalar("SELECT loan_id FROM banksms.transactions WHERE id = $1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if has_loan.is_some() {
        return Err(AppError::Conflict(
            "this transaction registered a loan as a whole; clear its category \
             before splitting"
                .into(),
        ));
    }

    insert_parts(&mut tx, &parent, &body.parts, &ctx.actor()).await?;
    sqlx::query(
        "UPDATE banksms.transactions
         SET split_at = now(), version = version + 1,
             edited_by = $1, edited_at = now(), updated_at = now()
         WHERE id = $2",
    )
    .bind(ctx.actor())
    .bind(id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    parts_response(pool.get_ref(), id).await
}

/// PUT /transactions/{id}/split — replace the part set (id may be the parent
/// or any child; the set is always edited as a whole).
pub async fn replace(
    pool: web::Data<PgPool>,
    req: HttpRequest,
    path: web::Path<i64>,
    body: web::Json<SplitBody>,
) -> AppResult<HttpResponse> {
    let ctx = super::ctx(&req)?;
    let id = path.into_inner();
    let mut tx = pool.begin().await?;

    let target = load_parent(&mut tx, id).await?;
    let parent_id = target.parent_id.unwrap_or(target.id);
    let parent = if parent_id == target.id {
        target
    } else {
        load_parent(&mut tx, parent_id).await?
    };
    check_if_match(&req, parent.version)?;
    if parent.split_at.is_none() {
        return Err(AppError::BadRequest("not split — use POST .../split first".into()));
    }

    remove_children(&mut tx, parent.id).await?;
    insert_parts(&mut tx, &parent, &body.parts, &ctx.actor()).await?;
    sqlx::query(
        "UPDATE banksms.transactions
         SET version = version + 1, edited_by = $1, edited_at = now(), updated_at = now()
         WHERE id = $2",
    )
    .bind(ctx.actor())
    .bind(parent.id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    parts_response(pool.get_ref(), parent.id).await
}

/// POST /transactions/{id}/unsplit — dissolve the split, restore the parent.
pub async fn unsplit(
    pool: web::Data<PgPool>,
    req: HttpRequest,
    path: web::Path<i64>,
) -> AppResult<HttpResponse> {
    let ctx = super::ctx(&req)?;
    let id = path.into_inner();
    let mut tx = pool.begin().await?;

    let target = load_parent(&mut tx, id).await?;
    let parent_id = target.parent_id.unwrap_or(target.id);
    let parent = if parent_id == target.id {
        target
    } else {
        load_parent(&mut tx, parent_id).await?
    };
    check_if_match(&req, parent.version)?;
    if parent.split_at.is_none() {
        return Err(AppError::BadRequest("not split".into()));
    }

    remove_children(&mut tx, parent.id).await?;
    sqlx::query(
        "UPDATE banksms.transactions
         SET split_at = NULL, version = version + 1,
             edited_by = $1, edited_at = now(), updated_at = now()
         WHERE id = $2",
    )
    .bind(ctx.actor())
    .bind(parent.id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    parts_response(pool.get_ref(), parent.id).await
}

/// GET /transactions/{id}/split — the whole set, for the editor. Accepts the
/// parent or any child id.
pub async fn get(pool: web::Data<PgPool>, path: web::Path<i64>) -> AppResult<HttpResponse> {
    let id = path.into_inner();
    let parent_id: Option<i64> = sqlx::query_scalar(
        "SELECT COALESCE(parent_id, id) FROM banksms.transactions
         WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool.get_ref())
    .await?;
    let parent_id = parent_id.ok_or_else(|| AppError::NotFound(format!("transaction {id}")))?;
    parts_response(pool.get_ref(), parent_id).await
}

async fn parts_response(pool: &PgPool, parent_id: i64) -> AppResult<HttpResponse> {
    let parent = super::transactions::view_by_id(pool, parent_id).await?;
    let children = super::transactions::views_by_parent(pool, parent_id).await?;
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "parent": parent,
        "parts": children,
    })))
}
