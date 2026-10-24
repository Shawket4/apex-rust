//! The classifier: what a bank payment was for and who it went to.
//!
//! Deterministic and explainable: every decision carries the rule that made
//! it and a sentence a person can check. At 95% or more it is written to the
//! transaction; below that it waits as a suggestion and the transaction is
//! left alone (except a person a label names, which is filled in).
//!
//! It learns from what people do, three ways:
//! - categories people give are the history the next payment to the same
//!   name is judged by (its own unreviewed output never counts);
//! - a correction to a labelled payee's payment takes the label's automatic
//!   say away;
//! - a rule people correct stops applying until accepted decisions put it
//!   back at 95%.
//!
//! It never registers a loan. An advance already in FalconGo's loans list is
//! linked by `linked_loan_id` (a reference, never modified) so the money
//! counts once; anything else in a posting category waits for a person.
//!
//! Runs after every poll that created transactions, and on demand with
//! `apex-rust classify` (all open transactions; `--dry-run` reports only).

pub mod engine;
pub mod fold;
pub mod model;

use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::Deserialize;
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::collections::{BTreeMap, HashSet};

use crate::errors::{AppError, AppResult};
pub use engine::{Decision, APPLY_AT};
pub use model::Model;

/// What a run did, for the CLI and the logs.
#[derive(Debug, Default)]
pub struct Report {
    /// (rule, applied?) → (count, total amount).
    pub by_rule: BTreeMap<(String, bool), (usize, Decimal)>,
    pub applied: usize,
    pub suggested: usize,
    /// Changed by someone between the read and the write; left for next time.
    pub skipped: usize,
    pub decisions: Vec<Decision>,
}

impl Report {
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{} applied, {} suggested, {} skipped (edited meanwhile)\n",
            self.applied, self.suggested, self.skipped
        );
        for ((rule, applied), (n, total)) in &self.by_rule {
            s.push_str(&format!(
                "  {:<16} {:<9} {:>5}  {:>16}\n",
                rule,
                if *applied { "applied" } else { "suggested" },
                n,
                engine::money(*total)
            ));
        }
        s
    }
}

/// Classify `ids` (or every open transaction), and write the result.
pub async fn run(pool: &PgPool, ids: Option<&[i64]>, dry_run: bool) -> AppResult<Report> {
    let model = Model::load(pool).await?;
    let targets: Option<HashSet<i64>> = ids.map(|v| v.iter().copied().collect());
    let decisions = engine::decide(&model, targets.as_ref());

    let mut report = Report::default();
    let amounts: std::collections::HashMap<i64, Decimal> =
        model.txns.iter().map(|t| (t.id, t.amount)).collect();
    for d in &decisions {
        let written = if dry_run {
            Some(d.apply)
        } else {
            write(pool, &model, d).await?
        };
        match written {
            Some(applied) => {
                if applied {
                    report.applied += 1;
                } else {
                    report.suggested += 1;
                }
                let e = report
                    .by_rule
                    .entry((d.rule.to_string(), applied))
                    .or_insert((0, Decimal::ZERO));
                e.0 += 1;
                e.1 += amounts.get(&d.txn_id).copied().unwrap_or_default();
            }
            None => report.skipped += 1,
        }
    }
    report.decisions = decisions;
    Ok(report)
}

/// Write one decision. `Some(true)` applied, `Some(false)` stored as a
/// suggestion, `None` when the transaction changed under us (a person edited
/// it, or another payment claimed the loan) and nothing was written.
async fn write(pool: &PgPool, model: &Model, d: &Decision) -> AppResult<Option<bool>> {
    let Some(t) = model.txns.iter().find(|t| t.id == d.txn_id) else {
        return Ok(None);
    };
    let mut tx = pool.begin().await?;

    let mut applied = d.apply;
    if d.apply && d.revise {
        // Its own earlier decision, overtaken: a refund. The money never
        // left, so no advance was paid by it either.
        let r = sqlx::query(
            "UPDATE banksms.transactions
             SET category = $1, linked_loan_id = NULL, version = version + 1, updated_at = now()
             WHERE id = $2 AND version = $3 AND category IS NOT DISTINCT FROM $4
               AND deleted_at IS NULL",
        )
        .bind(&d.category)
        .bind(d.txn_id)
        .bind(t.version)
        .bind(&t.category)
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() != 1 {
            return Ok(None);
        }
    } else if d.apply {
        let res = sqlx::query(
            "UPDATE banksms.transactions
             SET category = $1,
                 driver_id = CASE WHEN driver_id IS NULL AND employee_id IS NULL THEN $2 ELSE driver_id END,
                 employee_id = CASE WHEN driver_id IS NULL AND employee_id IS NULL THEN $3 ELSE employee_id END,
                 linked_loan_id = COALESCE($4, linked_loan_id),
                 version = version + 1, updated_at = now()
             WHERE id = $5 AND version = $6 AND category IS NULL AND deleted_at IS NULL",
        )
        .bind(&d.category)
        .bind(d.driver_id)
        .bind(d.employee_id)
        .bind(d.linked_loan_id)
        .bind(d.txn_id)
        .bind(t.version)
        .execute(&mut *tx)
        .await;
        match res {
            Ok(r) if r.rows_affected() == 1 => {}
            Ok(_) => return Ok(None),
            // Another payment linked this loan first.
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => {
                return Ok(None)
            }
            Err(e) => return Err(e.into()),
        }
    } else if d.fill_party && !t.has_party() && (d.driver_id.is_some() || d.employee_id.is_some()) {
        let r = sqlx::query(
            "UPDATE banksms.transactions
             SET driver_id = $1, employee_id = $2, version = version + 1, updated_at = now()
             WHERE id = $3 AND version = $4 AND driver_id IS NULL AND employee_id IS NULL
               AND deleted_at IS NULL",
        )
        .bind(d.driver_id)
        .bind(d.employee_id)
        .bind(d.txn_id)
        .bind(t.version)
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() != 1 {
            return Ok(None);
        }
        applied = false;
    }

    let status = if applied { "applied" } else { "suggested" };
    // A suggestion is refreshed every run; anything a person has decided,
    // and anything already applied, is final — except that a refund reopens
    // the question for the transfer it returns.
    let r = sqlx::query(
        r#"
        INSERT INTO banksms.classifications
            (transaction_id, status, rule, confidence, reason, category, driver_id,
             employee_id, linked_loan_id, payee_id, pair_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        ON CONFLICT (transaction_id) DO UPDATE SET
            status = EXCLUDED.status, rule = EXCLUDED.rule, confidence = EXCLUDED.confidence,
            reason = EXCLUDED.reason, category = EXCLUDED.category,
            driver_id = EXCLUDED.driver_id, employee_id = EXCLUDED.employee_id,
            linked_loan_id = EXCLUDED.linked_loan_id, payee_id = EXCLUDED.payee_id,
            pair_id = EXCLUDED.pair_id, updated_at = now(),
            decided_by = NULL, decided_at = NULL
        WHERE banksms.classifications.status = 'suggested'
           OR ($12 AND banksms.classifications.status = 'applied')
        "#,
    )
    .bind(d.txn_id)
    .bind(status)
    .bind(d.rule)
    .bind(
        Decimal::from_f64_retain(d.confidence)
            .unwrap_or_default()
            .round_dp(4),
    )
    .bind(&d.reason)
    .bind(&d.category)
    .bind(d.driver_id)
    .bind(d.employee_id)
    .bind(d.linked_loan_id)
    .bind(d.payee_id)
    .bind(d.pair_id)
    .bind(d.rule == "refunded")
    .execute(&mut *tx)
    .await?;
    if r.rows_affected() == 0 {
        // Settled meanwhile; leave it be.
        return Ok(None);
    }
    tx.commit().await?;
    Ok(Some(applied))
}

/// The fields a decision is about.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Verdict {
    pub category: Option<String>,
    pub driver_id: Option<i64>,
    pub employee_id: Option<i64>,
    pub linked_loan_id: Option<i64>,
}

/// A person saved a transaction: if that settles what the classifier applied
/// or suggested, record whether they took it or changed it. Edits that touch
/// none of the decision's fields (a description, a vehicle) decide nothing.
pub async fn record_edit(
    tx: &mut Transaction<'_, Postgres>,
    id: i64,
    before: &Verdict,
    after: &Verdict,
    actor: &str,
) -> AppResult<()> {
    if before == after {
        return Ok(());
    }
    let Some(r) = sqlx::query(
        "SELECT status, category, driver_id, employee_id, linked_loan_id
         FROM banksms.classifications WHERE transaction_id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(());
    };
    let status: String = r.get("status");
    if status != "applied" && status != "suggested" {
        return Ok(());
    }
    let proposed = Verdict {
        category: r.get("category"),
        driver_id: r.get("driver_id"),
        employee_id: r.get("employee_id"),
        linked_loan_id: r.get("linked_loan_id"),
    };
    let same_category = match (&proposed.category, &after.category) {
        (Some(p), Some(a)) => p.eq_ignore_ascii_case(a),
        (None, _) => true,
        _ => false,
    };
    let same_party = (proposed.driver_id.is_none() && proposed.employee_id.is_none())
        || (proposed.driver_id == after.driver_id && proposed.employee_id == after.employee_id);
    let same_link =
        proposed.linked_loan_id.is_none() || proposed.linked_loan_id == after.linked_loan_id;
    let verdict = if same_category && same_party && same_link {
        "accepted"
    } else {
        "corrected"
    };
    sqlx::query(
        "UPDATE banksms.classifications
         SET status = $1, decided_by = $2, decided_at = now(), updated_at = now()
         WHERE transaction_id = $3",
    )
    .bind(verdict)
    .bind(actor)
    .bind(id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// A person said no to a suggestion. Never suggested again.
pub async fn dismiss(pool: &PgPool, id: i64, actor: &str) -> AppResult<()> {
    let r = sqlx::query(
        "UPDATE banksms.classifications
         SET status = 'dismissed', decided_by = $1, decided_at = now(), updated_at = now()
         WHERE transaction_id = $2 AND status = 'suggested'",
    )
    .bind(actor)
    .bind(id)
    .execute(pool)
    .await?;
    if r.rows_affected() == 0 {
        return Err(AppError::NotFound(format!(
            "no open suggestion for transaction {id}"
        )));
    }
    Ok(())
}

/* ------------------------------ payees ----------------------------------- */

/// The import file: payees and every spelling the banks use for them.
#[derive(Debug, Deserialize)]
pub struct PayeeFile {
    pub payees: Vec<PayeeIn>,
}

#[derive(Debug, Deserialize)]
pub struct PayeeIn {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub consistency: Option<String>,
    #[serde(default)]
    pub driver_id: Option<i64>,
    #[serde(default)]
    pub employee_id: Option<i64>,
    #[serde(default)]
    pub notes: Option<String>,
    pub aliases: Vec<String>,
}

/// Load payees (`apex-rust import-payees FILE`). A payee whose spelling is
/// already known is updated in place, so the file can be re-imported after
/// more labelling. All or nothing.
pub async fn import_payees(
    pool: &PgPool,
    file: &PayeeFile,
    actor: &str,
) -> AppResult<(usize, usize)> {
    let mut tx = pool.begin().await?;
    let (mut payees, mut aliases) = (0usize, 0usize);
    for p in &file.payees {
        let bad = |m: String| AppError::BadRequest(format!("payee '{}': {m}", p.name));
        let consistency = p.consistency.as_deref().unwrap_or("varies");
        if !matches!(consistency, "always" | "varies") {
            return Err(bad(format!(
                "consistency must be always or varies, not '{consistency}'"
            )));
        }
        if p.aliases.is_empty() {
            return Err(bad("needs at least one spelling".into()));
        }
        if let Some(c) = p.category.as_deref().filter(|c| !c.is_empty()) {
            let known: Option<String> = sqlx::query_scalar(
                "SELECT key FROM banksms.categories WHERE lower(key) = lower($1)",
            )
            .bind(c)
            .fetch_optional(&mut *tx)
            .await?;
            if known.is_none() {
                return Err(bad(format!("unknown category '{c}'")));
            }
        }
        let keys: Vec<String> = p
            .aliases
            .iter()
            .map(|a| fold::fold(a))
            .filter(|k| !k.is_empty())
            .collect();
        let existing: Option<i64> = sqlx::query_scalar(
            "SELECT payee_id FROM banksms.payee_aliases WHERE key = ANY($1) LIMIT 1",
        )
        .bind(&keys)
        .fetch_optional(&mut *tx)
        .await?;
        let category = p.category.clone().filter(|c| !c.is_empty());
        let id: i64 = match existing {
            Some(id) => {
                sqlx::query(
                    "UPDATE banksms.payees SET name = $1, kind = $2, category = $3, consistency = $4,
                         driver_id = $5, employee_id = $6, notes = $7, updated_at = now()
                     WHERE id = $8",
                )
                .bind(&p.name)
                .bind(&p.kind)
                .bind(&category)
                .bind(consistency)
                .bind(p.driver_id)
                .bind(p.employee_id)
                .bind(&p.notes)
                .bind(id)
                .execute(&mut *tx)
                .await?;
                id
            }
            None => {
                sqlx::query_scalar(
                    "INSERT INTO banksms.payees
                         (name, kind, category, consistency, driver_id, employee_id, notes, created_by)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
                )
                .bind(&p.name)
                .bind(&p.kind)
                .bind(&category)
                .bind(consistency)
                .bind(p.driver_id)
                .bind(p.employee_id)
                .bind(&p.notes)
                .bind(actor)
                .fetch_one(&mut *tx)
                .await?
            }
        };
        payees += 1;
        for (spelling, key) in p
            .aliases
            .iter()
            .zip(p.aliases.iter().map(|a| fold::fold(a)))
        {
            if key.is_empty() {
                continue;
            }
            sqlx::query(
                "INSERT INTO banksms.payee_aliases (key, payee_id, spelling) VALUES ($1, $2, $3)
                 ON CONFLICT (key) DO UPDATE SET payee_id = EXCLUDED.payee_id, spelling = EXCLUDED.spelling",
            )
            .bind(&key)
            .bind(id)
            .bind(spelling.trim())
            .execute(&mut *tx)
            .await?;
            aliases += 1;
        }
    }
    tx.commit().await?;
    Ok((payees, aliases))
}

/// For logs: a decision in one line.
pub fn describe(d: &Decision) -> String {
    format!(
        "#{} {} {:.0}% {} — {}",
        d.txn_id,
        d.rule,
        d.confidence * 100.0,
        d.category.as_deref().unwrap_or("(person only)"),
        d.reason
    )
}

/// The confidence as stored (4 decimals) and back, for views.
pub fn confidence_f64(d: Decimal) -> f64 {
    d.to_f64().unwrap_or(0.0)
}
