//! Registering a categorised transaction into FalconGo's `public.loans`.
//!
//! Marking a payment as Advance / Loan / Part-of-salary against a driver or
//! employee is what makes it reach the loans ledger FalconGo still serves.
//!
//! # The invariant (carried over from the previous implementation)
//!
//! > apex-rust only ever modifies `loans` rows it created itself, and
//! > `transactions.loan_id` records exactly which those are.
//!
//! A loan without a transaction pointing at it is FalconGo's and is never
//! touched. Direction is one-way: transaction → loan. Loans edited in FalconGo
//! are never read back as truth.
//!
//! # Sync rule (deliberate change from the old code)
//!
//! The old implementation never refreshed an already-posted loan, which left
//! the transaction and the loan silently divergent after an edit. Now: while
//! the loan is unpaid, edits keep it in sync; once `is_paid`, any
//! money-relevant change is refused with a clear 409 — a settled loan must be
//! unsettled in FalconGo first.

use rust_decimal::Decimal;
use serde::Serialize;
use sqlx::{Postgres, Row, Transaction};

use crate::errors::{AppError, AppResult};

/// What a category demands, looked up by key (case-insensitive).
#[derive(Debug, Clone)]
pub struct CategoryRule {
    pub key: String,
    /// Non-NULL means "registering into loans with this kind". One of
    /// advance | loan | salary, matching `public.loans.kind`'s CHECK.
    pub posting_kind: Option<String>,
    pub required_party: String, // none | driver | employee | either
}

/// The slice of a transaction that registration cares about.
#[derive(Debug, Clone)]
pub struct Registrable {
    pub id: i64,
    pub amount: Decimal,
    pub occurred_at: chrono::DateTime<chrono::Utc>,
    pub description: Option<String>,
    pub category: Option<String>,
    pub driver_id: Option<i64>,
    pub employee_id: Option<i64>,
    pub loan_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoanInfo {
    pub id: i64,
    pub kind: String,
    pub is_paid: bool,
}

pub async fn load_rule(
    tx: &mut Transaction<'_, Postgres>,
    category: Option<&str>,
) -> AppResult<Option<CategoryRule>> {
    let Some(category) = category.filter(|c| !c.is_empty()) else {
        return Ok(None);
    };
    let row = sqlx::query(
        "SELECT key, posting_kind, required_party
         FROM banksms.categories
         WHERE lower(key) = lower($1) AND enabled",
    )
    .bind(category)
    .fetch_optional(&mut **tx)
    .await?;

    Ok(row.map(|r| CategoryRule {
        key: r.get("key"),
        posting_kind: r.get("posting_kind"),
        required_party: r.get("required_party"),
    }))
}

/// Reject a write whose category demands a person it doesn't have. Runs on
/// every create/patch so the user hears "an advance needs a person" at save
/// time, not by discovering that registration silently did nothing.
pub fn validate(rule: Option<&CategoryRule>, t: &Registrable) -> AppResult<()> {
    let Some(rule) = rule else { return Ok(()) };
    let satisfied = match rule.required_party.as_str() {
        "driver" => t.driver_id.is_some(),
        "employee" => t.employee_id.is_some(),
        "either" => t.driver_id.is_some() || t.employee_id.is_some(),
        _ => true,
    };
    if !satisfied {
        return Err(AppError::BadRequest(format!(
            "category '{}' requires a {}",
            rule.key,
            match rule.required_party.as_str() {
                "driver" => "driver",
                "employee" => "employee",
                _ => "driver or employee",
            }
        )));
    }
    Ok(())
}

/// Bring the registered state in line with the transaction. Idempotent.
///
///   should be registered and is not → insert the loan, record loan_id
///   should not be and is            → soft-delete the loan (unless paid)
///   both, unpaid                    → sync amount/kind/party/date
///   both, paid, money changed       → 409
pub async fn reconcile(
    tx: &mut Transaction<'_, Postgres>,
    t: &Registrable,
) -> AppResult<Option<LoanInfo>> {
    let rule = load_rule(tx, t.category.as_deref()).await?;
    let kind = rule.as_ref().and_then(|r| r.posting_kind.clone());
    let has_party = t.driver_id.is_some() || t.employee_id.is_some();
    let should_post = kind.is_some() && has_party && t.amount > Decimal::ZERO;

    match (should_post, t.loan_id) {
        (false, None) => Ok(None),

        (true, None) => {
            let kind = kind.unwrap();
            let loan_id = insert_loan(tx, t, &kind).await?;
            sqlx::query("UPDATE banksms.transactions SET loan_id = $1 WHERE id = $2")
                .bind(loan_id)
                .bind(t.id)
                .execute(&mut **tx)
                .await?;
            log::info!(
                "registered transaction {} as {kind} loan {loan_id} (driver={:?}, employee={:?})",
                t.id,
                t.driver_id,
                t.employee_id
            );
            Ok(Some(LoanInfo {
                id: loan_id,
                kind,
                is_paid: false,
            }))
        }

        (false, Some(loan_id)) => {
            let unregistered = sqlx::query(
                "UPDATE public.loans SET deleted_at = now()
                 WHERE id = $1 AND deleted_at IS NULL AND is_paid = false",
            )
            .bind(loan_id)
            .execute(&mut **tx)
            .await?;
            if unregistered.rows_affected() == 0 {
                // Already gone is fine; still-paid is not.
                let paid: Option<bool> = sqlx::query_scalar(
                    "SELECT is_paid FROM public.loans WHERE id = $1 AND deleted_at IS NULL",
                )
                .bind(loan_id)
                .fetch_optional(&mut **tx)
                .await?;
                if paid == Some(true) {
                    return Err(AppError::Conflict(
                        "registered loan is already settled; unsettle it in FalconGo first"
                            .to_string(),
                    ));
                }
            }
            sqlx::query("UPDATE banksms.transactions SET loan_id = NULL WHERE id = $1")
                .bind(t.id)
                .execute(&mut **tx)
                .await?;
            log::info!(
                "un-registered transaction {} (soft-deleted loan {loan_id})",
                t.id
            );
            Ok(None)
        }

        (true, Some(loan_id)) => {
            let kind = kind.unwrap();
            // Sync while unpaid. `date` is FalconGo's TEXT YYYY-MM-DD in Cairo
            // time — UTC would file a late-evening advance under the previous day.
            let synced = sqlx::query(
                r#"
                UPDATE public.loans
                SET amount = $1::numeric, kind = $2,
                    driver_id = $3, employee_id = $4,
                    date = to_char($5::timestamptz AT TIME ZONE 'Africa/Cairo', 'YYYY-MM-DD'),
                    description = COALESCE(NULLIF($6, ''), description),
                    updated_at = now()
                WHERE id = $7 AND deleted_at IS NULL AND is_paid = false
                "#,
            )
            .bind(t.amount)
            .bind(&kind)
            .bind(t.driver_id)
            .bind(t.employee_id)
            .bind(t.occurred_at)
            .bind(t.description.as_deref().unwrap_or(""))
            .bind(loan_id)
            .execute(&mut **tx)
            .await?;

            if synced.rows_affected() > 0 {
                return Ok(Some(LoanInfo {
                    id: loan_id,
                    kind,
                    is_paid: false,
                }));
            }

            // Paid or gone. Divergence on a settled loan is refused, not hidden.
            let row = sqlx::query(
                "SELECT amount::numeric AS amount, is_paid, kind
                 FROM public.loans WHERE id = $1 AND deleted_at IS NULL",
            )
            .bind(loan_id)
            .fetch_optional(&mut **tx)
            .await?;
            match row {
                Some(r) if r.get::<bool, _>("is_paid") => {
                    let loan_amount: Decimal = r.get("amount");
                    if loan_amount != t.amount || r.get::<String, _>("kind") != kind {
                        Err(AppError::Conflict(
                            "registered loan is already settled; unsettle it in FalconGo first"
                                .to_string(),
                        ))
                    } else {
                        Ok(Some(LoanInfo {
                            id: loan_id,
                            kind,
                            is_paid: true,
                        }))
                    }
                }
                // The loan vanished under us (FalconGo delete). Re-create.
                _ => {
                    let new_id = insert_loan(tx, t, &kind).await?;
                    sqlx::query("UPDATE banksms.transactions SET loan_id = $1 WHERE id = $2")
                        .bind(new_id)
                        .bind(t.id)
                        .execute(&mut **tx)
                        .await?;
                    Ok(Some(LoanInfo {
                        id: new_id,
                        kind,
                        is_paid: false,
                    }))
                }
            }
        }
    }
}

/// Cascade a transaction's soft delete to its registered loan. A paid loan
/// blocks the delete — an orphaned settled deduction is exactly the silent
/// wrongness this module exists to prevent.
pub async fn unregister_for_delete(
    tx: &mut Transaction<'_, Postgres>,
    t: &Registrable,
) -> AppResult<()> {
    let Some(loan_id) = t.loan_id else {
        return Ok(());
    };
    let paid: Option<bool> =
        sqlx::query_scalar("SELECT is_paid FROM public.loans WHERE id = $1 AND deleted_at IS NULL")
            .bind(loan_id)
            .fetch_optional(&mut **tx)
            .await?;
    if paid == Some(true) {
        return Err(AppError::Conflict(
            "this transaction registered a loan that is already settled; \
             unsettle it in FalconGo first"
                .to_string(),
        ));
    }
    sqlx::query("UPDATE public.loans SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL")
        .bind(loan_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Insert the loan row. `method='banksms'` is the stable marker that makes
/// these rows identifiable from the FalconGo side too.
async fn insert_loan(
    tx: &mut Transaction<'_, Postgres>,
    t: &Registrable,
    kind: &str,
) -> AppResult<i64> {
    let row = sqlx::query(
        r#"
        INSERT INTO public.loans
            (created_at, updated_at, amount, method, date,
             driver_id, employee_id, is_paid, description, kind)
        VALUES (
            now(), now(), $1::numeric, 'banksms',
            to_char($2::timestamptz AT TIME ZONE 'Africa/Cairo', 'YYYY-MM-DD'),
            $3, $4, false, $5, $6
        )
        RETURNING id::bigint AS id
        "#,
    )
    .bind(t.amount)
    .bind(t.occurred_at)
    .bind(t.driver_id)
    .bind(t.employee_id)
    .bind(
        t.description
            .as_deref()
            .filter(|d| !d.trim().is_empty())
            .unwrap_or("Recorded from a bank message"),
    )
    .bind(kind)
    .fetch_one(&mut **tx)
    .await?;

    row.try_get::<i64, _>("id")
        .map_err(|e| AppError::Internal(format!("could not read new loan id: {e}")))
}

/// Loan info for display on reads.
pub async fn loan_info(pool: &sqlx::PgPool, loan_id: Option<i64>) -> AppResult<Option<LoanInfo>> {
    let Some(id) = loan_id else { return Ok(None) };
    let row = sqlx::query(
        "SELECT id::bigint AS id, kind, is_paid FROM public.loans
         WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| LoanInfo {
        id: r.get("id"),
        kind: r.get("kind"),
        is_paid: r.get("is_paid"),
    }))
}
