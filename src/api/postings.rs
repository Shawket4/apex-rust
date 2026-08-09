//! Materialising a categorised transaction into FalconGo's `public.loans`.
//!
//! An "advance" recorded here has to reach payroll, and payroll reads
//! `public.loans`. So this module writes there — carefully.
//!
//! # The invariant
//!
//! > apex-rust only ever modifies `loans` rows it created itself, and
//! > `banksms.transaction_postings` records exactly which those are.
//!
//! Every function below preserves it. A loan with no posting row is FalconGo's
//! and is never touched, read-modify-write or otherwise. That is what keeps two
//! writers on one table from corrupting each other.
//!
//! # When posting happens
//!
//! On **verification**, not on categorisation. A bank SMS that was mis-parsed
//! must not be able to quietly create a payroll deduction; requiring a human to
//! confirm the amount first costs one click and removes that entire class of
//! error. Un-verifying reverses the posting.
//!
//! # Direction
//!
//! One way only: transaction → loan. If someone edits the loan in FalconGo we do
//! not fight them, and we never read it back as truth.

use log::{info, warn};
use rust_decimal::Decimal;
use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::errors::{AppError, AppResult};

/// The subset of a transaction that posting cares about.
#[derive(Debug, Clone)]
pub struct Postable {
    pub id: i64,
    pub amount: Option<Decimal>,
    pub occurred_at: Option<chrono::DateTime<chrono::Utc>>,
    pub description: Option<String>,
    pub category: Option<String>,
    pub driver_id: Option<i64>,
    pub employee_id: Option<i64>,
    pub verified: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostingTarget {
    None,
    Loan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartyRequirement {
    None,
    Driver,
    Employee,
    Either,
}

impl PartyRequirement {
    fn parse(s: &str) -> Self {
        match s {
            "driver" => Self::Driver,
            "employee" => Self::Employee,
            "either" => Self::Either,
            _ => Self::None,
        }
    }

    /// Whether the party actually attached satisfies this requirement.
    pub fn satisfied_by(&self, driver_id: Option<i64>, employee_id: Option<i64>) -> bool {
        match self {
            Self::None => true,
            Self::Driver => driver_id.is_some(),
            Self::Employee => employee_id.is_some(),
            Self::Either => driver_id.is_some() || employee_id.is_some(),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Driver => "driver",
            Self::Employee => "employee",
            Self::Either => "either",
        }
    }
}

/// What a category demands, looked up by its key.
#[derive(Debug, Clone)]
pub struct CategoryRule {
    pub key: String,
    pub posting_target: PostingTarget,
    pub required_party: PartyRequirement,
}

pub async fn load_rule(
    tx: &mut Transaction<'_, Postgres>,
    category: Option<&str>,
) -> AppResult<Option<CategoryRule>> {
    let Some(category) = category.filter(|c| !c.is_empty()) else {
        return Ok(None);
    };

    let row = sqlx::query(
        "SELECT key, posting_target::text AS posting_target, required_party::text AS required_party
         FROM banksms.categories
         WHERE lower(key) = lower($1) AND enabled",
    )
    .bind(category)
    .fetch_optional(&mut **tx)
    .await?;

    Ok(row.map(|r| CategoryRule {
        key: r.get("key"),
        posting_target: match r.get::<String, _>("posting_target").as_str() {
            "loan" => PostingTarget::Loan,
            _ => PostingTarget::None,
        },
        required_party: PartyRequirement::parse(&r.get::<String, _>("required_party")),
    }))
}

/// Validate a transaction against its category's requirements.
///
/// Runs on every write, not just on verify, so the user is told immediately that
/// an advance needs a person rather than discovering it when posting silently
/// does nothing.
pub fn validate(rule: &CategoryRule, t: &Postable) -> AppResult<()> {
    if !rule.required_party.satisfied_by(t.driver_id, t.employee_id) {
        return Err(AppError::BadRequest(format!(
            "category '{}' requires a {}",
            rule.key,
            match rule.required_party {
                PartyRequirement::Driver => "driver",
                PartyRequirement::Employee => "employee",
                PartyRequirement::Either => "driver or employee",
                PartyRequirement::None => "party",
            }
        )));
    }
    Ok(())
}

/// Bring the posted state in line with the transaction's current state.
///
/// Idempotent and safe to call after any write:
///   * should be posted and is not  -> insert the loan + link row
///   * should not be posted and is  -> soft-delete the loan, drop the link
///   * already correct              -> no-op
///
/// Returns the loan id if one is now posted.
///
/// The amount is deliberately NOT refreshed on an already-posted loan. An edit
/// weeks later must not silently reopen a settled payslip; changing a posted
/// amount requires un-verifying and re-verifying, which is visible.
pub async fn reconcile(
    tx: &mut Transaction<'_, Postgres>,
    t: &Postable,
    actor: &str,
) -> AppResult<Option<i64>> {
    let rule = load_rule(tx, t.category.as_deref()).await?;

    let should_post = matches!(
        rule.as_ref().map(|r| r.posting_target),
        Some(PostingTarget::Loan)
    ) && t.verified
        && (t.driver_id.is_some() || t.employee_id.is_some())
        && t.amount.map(|a| a > Decimal::ZERO).unwrap_or(false);

    let existing: Option<(String, i64)> = sqlx::query_as(
        "SELECT target_table, target_id FROM banksms.transaction_postings
         WHERE transaction_id = $1",
    )
    .bind(t.id)
    .fetch_optional(&mut **tx)
    .await?;

    match (should_post, existing) {
        // Already correct.
        (true, Some((_, id))) => Ok(Some(id)),

        // Post it.
        (true, None) => {
            let loan_id = insert_loan(tx, t).await?;
            sqlx::query(
                "INSERT INTO banksms.transaction_postings
                     (transaction_id, target_table, target_id, posted_by)
                 VALUES ($1, 'loans', $2, $3)",
            )
            .bind(t.id)
            .bind(loan_id)
            .bind(actor)
            .execute(&mut **tx)
            .await?;

            info!(
                "posted transaction {} to loans {} (driver={:?}, employee={:?})",
                t.id, loan_id, t.driver_id, t.employee_id
            );
            Ok(Some(loan_id))
        }

        // Un-post it. Soft delete only: GORM honours deleted_at, and a hard
        // delete would destroy a row payroll may already have reported on.
        (false, Some((table, id))) => {
            if table != "loans" {
                warn!("unknown posting target {table} for transaction {}", t.id);
                return Ok(None);
            }
            sqlx::query("UPDATE public.loans SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL")
                .bind(id)
                .execute(&mut **tx)
                .await?;
            sqlx::query("DELETE FROM banksms.transaction_postings WHERE transaction_id = $1")
                .bind(t.id)
                .execute(&mut **tx)
                .await?;
            info!("un-posted transaction {} (soft-deleted loan {id})", t.id);
            Ok(None)
        }

        (false, None) => Ok(None),
    }
}

/// Insert the loan row.
///
/// `loans.date` is TEXT holding `YYYY-MM-DD` (FalconGo's convention), and the
/// business date is the Cairo calendar day -- using UTC here would file a late
/// evening advance under the previous day.
async fn insert_loan(tx: &mut Transaction<'_, Postgres>, t: &Postable) -> AppResult<i64> {
    let row = sqlx::query(
        r#"
        INSERT INTO public.loans
            (created_at, updated_at, amount, method, date,
             driver_id, employee_id, is_paid, description)
        VALUES (
            now(), now(), $1, $2,
            to_char(COALESCE($3::timestamptz, now()) AT TIME ZONE 'Africa/Cairo', 'YYYY-MM-DD'),
            $4, $5, false, $6
        )
        -- loans.id is INT4, not INT8. Cast at the boundary so the Rust side has
        -- one integer width to reason about.
        RETURNING id::bigint AS id
        "#,
    )
    .bind(t.amount)
    // `method` is FalconGo's free-text column. A stable machine-readable marker
    // makes these rows identifiable from the FalconGo side too, without relying
    // on our link table being reachable.
    .bind("banksms")
    .bind(t.occurred_at)
    .bind(t.driver_id)
    .bind(t.employee_id)
    .bind(
        t.description
            .as_deref()
            .filter(|d| !d.trim().is_empty())
            .unwrap_or("Advance recorded from a bank message"),
    )
    .fetch_one(&mut **tx)
    .await?;

    // try_get, not get: a decode mismatch here would otherwise panic the worker
    // mid-request rather than returning an error the caller can see.
    row.try_get::<i64, _>("id")
        .map_err(|e| AppError::Internal(format!("could not read new loan id: {e}")))
}

/// Cascade a transaction's soft delete to its posted loan.
///
/// Without this, deleting a transaction leaves an orphan payroll deduction that
/// nothing in the UI can explain.
pub async fn unpost_for_deleted(
    tx: &mut Transaction<'_, Postgres>,
    transaction_id: i64,
) -> AppResult<()> {
    let existing: Option<(String, i64)> = sqlx::query_as(
        "SELECT target_table, target_id FROM banksms.transaction_postings
         WHERE transaction_id = $1",
    )
    .bind(transaction_id)
    .fetch_optional(&mut **tx)
    .await?;

    if let Some((table, id)) = existing {
        if table == "loans" {
            sqlx::query(
                "UPDATE public.loans SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL",
            )
            .bind(id)
            .execute(&mut **tx)
            .await?;
        }
        sqlx::query("DELETE FROM banksms.transaction_postings WHERE transaction_id = $1")
            .bind(transaction_id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// Where a transaction was posted, for display.
pub async fn posting_for(pool: &PgPool, transaction_id: i64) -> AppResult<Option<(String, i64)>> {
    Ok(sqlx::query_as(
        "SELECT target_table, target_id FROM banksms.transaction_postings
         WHERE transaction_id = $1",
    )
    .bind(transaction_id)
    .fetch_optional(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn postable() -> Postable {
        Postable {
            id: 1,
            amount: Some(dec!(1500)),
            occurred_at: None,
            description: None,
            category: Some("Advance".into()),
            driver_id: Some(7),
            employee_id: None,
            verified: true,
        }
    }

    fn rule(target: PostingTarget, party: PartyRequirement) -> CategoryRule {
        CategoryRule {
            key: "Advance".into(),
            posting_target: target,
            required_party: party,
        }
    }

    #[test]
    fn either_accepts_driver_or_employee_but_not_neither() {
        let r = PartyRequirement::Either;
        assert!(r.satisfied_by(Some(1), None));
        assert!(r.satisfied_by(None, Some(2)));
        assert!(!r.satisfied_by(None, None));
    }

    #[test]
    fn driver_requirement_is_not_satisfied_by_an_employee() {
        // The two are different tables in FalconGo; accepting one for the other
        // would post a loan against the wrong person.
        let r = PartyRequirement::Driver;
        assert!(r.satisfied_by(Some(1), None));
        assert!(!r.satisfied_by(None, Some(2)));
    }

    #[test]
    fn none_requirement_accepts_anything() {
        let r = PartyRequirement::None;
        assert!(r.satisfied_by(None, None));
        assert!(r.satisfied_by(Some(1), None));
    }

    #[test]
    fn validation_rejects_a_posting_category_without_a_party() {
        let mut t = postable();
        t.driver_id = None;
        let err = validate(&rule(PostingTarget::Loan, PartyRequirement::Either), &t);
        assert!(err.is_err());
        let msg = err.unwrap_err().to_string();
        assert!(msg.contains("driver or employee"), "unhelpful message: {msg}");
    }

    #[test]
    fn validation_passes_when_the_party_is_present() {
        assert!(validate(&rule(PostingTarget::Loan, PartyRequirement::Either), &postable()).is_ok());
    }

    #[test]
    fn plain_categories_need_no_party() {
        let mut t = postable();
        t.driver_id = None;
        t.category = Some("Fuel".into());
        assert!(validate(&rule(PostingTarget::None, PartyRequirement::None), &t).is_ok());
    }
}
