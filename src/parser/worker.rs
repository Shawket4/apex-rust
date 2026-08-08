//! Applies the parser to stored messages and materialises transactions.
//!
//! Runs after each poll cycle, and on demand via the admin reparse endpoints.
//! Parsing is deliberately separate from ingestion: a parser bug can never cost
//! us a message, because the raw body is already durable before this runs.

use log::{info, warn};
use std::collections::HashSet;

use crate::errors::AppResult;
use crate::parser::{self, templates, ParseOutcome, ParseStatus};

#[derive(Debug, Default, Clone, Copy)]
pub struct ParseRun {
    pub examined: usize,
    pub parsed: usize,
    pub partial: usize,
    pub unmatched: usize,
    pub ignored: usize,
    pub transactions_created: usize,
}

/// Which rows a reparse should touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Newly ingested messages that have never been parsed.
    Pending,
    /// Everything the parser previously failed to fully understand, plus rows it
    /// ignored. Deliberately EXCLUDES `parsed` rows -- adding a template must not
    /// re-derive money that already reconciled.
    Reviewable,
    /// Everything, including previously `parsed` rows. Only for an explicit
    /// admin reparse after a parser fix.
    All,
}

impl Scope {
    fn sql_filter(&self) -> &'static str {
        match self {
            Scope::Pending => "parse_status = 'pending'",
            Scope::Reviewable => {
                "parse_status IN ('pending', 'partial', 'unmatched', 'ignored', 'error')"
            }
            Scope::All => "TRUE",
        }
    }
}

/// Parse messages in `scope`, writing back status and creating transactions.
///
/// Drains the scope in batches of `batch_size` rather than stopping after one
/// batch, up to `max_rows`. Without draining, a backfill of 731 messages would
/// only parse the first batch per poll cycle and take many minutes to catch up.
///
/// Paging is by ascending `id`, NOT by re-running the status filter. `Reviewable`
/// and `All` include statuses that rows keep after processing (an `ignored`
/// message stays `ignored`), so a filter-based loop would re-select the same rows
/// forever.
pub async fn run(pool: &sqlx::PgPool, scope: Scope, max_rows: i64) -> AppResult<ParseRun> {
    let compiled = templates::load(pool).await?;
    let noise: HashSet<String> = parser::load_noise_skeletons(pool).await?;

    let batch_size: i64 = 500;
    let mut after_id: i64 = 0;
    let mut run = ParseRun::default();

    loop {
        let sql = format!(
            "SELECT id, body FROM banksms.raw_messages \
             WHERE ({}) AND id > $1 ORDER BY id ASC LIMIT $2",
            scope.sql_filter()
        );
        let rows: Vec<(i64, String)> = sqlx::query_as(&sql)
            .bind(after_id)
            .bind(batch_size.min(max_rows - run.examined as i64))
            .fetch_all(pool)
            .await?;

        if rows.is_empty() {
            break;
        }

        after_id = rows.last().map(|(id, _)| *id).unwrap_or(after_id);
        let batch_len = rows.len();

        process_batch(pool, rows, &compiled, &noise, &mut run).await?;

        if batch_len < batch_size as usize || run.examined as i64 >= max_rows {
            break;
        }
    }

    if run.examined > 0 {
        info!(
            "parse run ({:?}): {} examined, {} parsed, {} partial, {} unmatched, {} ignored, {} transactions",
            scope, run.examined, run.parsed, run.partial, run.unmatched, run.ignored,
            run.transactions_created
        );
    }

    Ok(run)
}

async fn process_batch(
    pool: &sqlx::PgPool,
    rows: Vec<(i64, String)>,
    compiled: &[templates::CompiledTemplate],
    noise: &HashSet<String>,
    run: &mut ParseRun,
) -> AppResult<()> {
    for (raw_id, body) in rows {
        run.examined += 1;
        let outcome = parser::parse(
            &body,
            compiled,
            noise,
            crate::parser::triage::DEFAULT_THRESHOLD,
        );

        match outcome.status {
            ParseStatus::Parsed => run.parsed += 1,
            ParseStatus::Partial => run.partial += 1,
            ParseStatus::Unmatched => run.unmatched += 1,
            ParseStatus::Ignored => run.ignored += 1,
        }

        // Status write-back and transaction upsert share a transaction, so a row
        // can never be marked parsed without its transaction existing.
        let mut tx = pool.begin().await?;

        sqlx::query(
            r#"
            UPDATE banksms.raw_messages
            SET parse_status   = $2::text::banksms.parse_status,
                parser_version = $3,
                skeleton_hash  = $4,
                triage_score   = $5,
                parsed_at      = now(),
                parse_error    = NULL
            WHERE id = $1
            "#,
        )
        .bind(raw_id)
        .bind(outcome.status.as_str())
        .bind(outcome.parser_version)
        .bind(&outcome.skeleton_hash)
        .bind(outcome.triage_score)
        .execute(&mut *tx)
        .await?;

        if outcome.yields_transaction() {
            let created = upsert_transaction(&mut tx, raw_id, &outcome).await?;
            if created {
                run.transactions_created += 1;
            }
        }

        tx.commit().await?;
    }

    Ok(())
}

/// Insert or refresh the transaction for a parsed message.
///
/// The re-parse rule lives here: `parsed_*` columns are rebuilt ONLY where
/// `source = 'whatsapp'`. Manual and imported rows keep their values even if a
/// raw message is later linked to them, and user-owned columns (category,
/// verified) plus overrides, notes and tags are never touched.
async fn upsert_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    raw_id: i64,
    o: &ParseOutcome,
) -> AppResult<bool> {
    let result = sqlx::query(
        r#"
        INSERT INTO banksms.transactions (
            source, raw_message_id,
            parsed_direction, parsed_amount, parsed_currency, parsed_account,
            parsed_counterparty, parsed_reference, parsed_balance_after,
            parsed_occurred_at, parsed_template, parser_version,
            confidence, parse_method
        )
        VALUES (
            'whatsapp', $1,
            $2::text::banksms.direction, $3, $4, $5,
            $6, $7, $8,
            $9, $10, $11,
            $12, $13::text::banksms.parse_method
        )
        ON CONFLICT (raw_message_id) WHERE raw_message_id IS NOT NULL
        DO UPDATE SET
            parsed_direction     = EXCLUDED.parsed_direction,
            parsed_amount        = EXCLUDED.parsed_amount,
            parsed_currency      = EXCLUDED.parsed_currency,
            parsed_account       = EXCLUDED.parsed_account,
            parsed_counterparty  = EXCLUDED.parsed_counterparty,
            parsed_reference     = EXCLUDED.parsed_reference,
            parsed_balance_after = EXCLUDED.parsed_balance_after,
            parsed_occurred_at   = EXCLUDED.parsed_occurred_at,
            parsed_template      = EXCLUDED.parsed_template,
            parser_version       = EXCLUDED.parser_version,
            confidence           = EXCLUDED.confidence,
            parse_method         = EXCLUDED.parse_method,
            version              = banksms.transactions.version + 1,
            updated_at           = now()
        WHERE banksms.transactions.source = 'whatsapp'
        "#,
    )
    .bind(raw_id)
    .bind(o.direction.map(|d| d.as_str()))
    .bind(o.amount)
    .bind(o.currency.as_deref())
    .bind(o.account.as_deref())
    .bind(o.counterparty.as_deref())
    .bind(o.reference.as_deref())
    .bind(o.balance_after)
    .bind(o.occurred_at)
    .bind(o.template.as_deref())
    .bind(o.parser_version)
    .bind(o.confidence)
    .bind(o.method.map(|m| m.as_str()))
    .execute(&mut **tx)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// Mark a message's skeleton as noise and re-ignore every message sharing it.
///
/// This is the half of the feedback loop that stops the review queue refilling
/// with the same false positive after each reparse.
pub async fn mark_skeleton_as_noise(
    pool: &sqlx::PgPool,
    raw_id: i64,
    actor: &str,
) -> AppResult<u64> {
    let mut tx = pool.begin().await?;

    let row: Option<(Option<String>, String)> =
        sqlx::query_as("SELECT skeleton_hash, body FROM banksms.raw_messages WHERE id = $1")
            .bind(raw_id)
            .fetch_optional(&mut *tx)
            .await?;

    let Some((Some(hash), body)) = row else {
        warn!("cannot mark raw message {raw_id} as noise: no skeleton hash");
        return Ok(0);
    };

    sqlx::query(
        r#"
        INSERT INTO banksms.noise_skeletons (skeleton_hash, example_body, marked_by)
        VALUES ($1, $2, $3)
        ON CONFLICT (skeleton_hash) DO NOTHING
        "#,
    )
    .bind(&hash)
    .bind(&body)
    .bind(actor)
    .execute(&mut *tx)
    .await?;

    // Re-ignore siblings, and soft-delete any transactions they produced -- a row
    // confirmed as noise should stop counting as money immediately.
    let affected = sqlx::query(
        r#"
        UPDATE banksms.raw_messages
        SET parse_status = 'ignored', parsed_at = now()
        WHERE skeleton_hash = $1 AND parse_status <> 'ignored'
        "#,
    )
    .bind(&hash)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    sqlx::query(
        r#"
        UPDATE banksms.transactions t
        SET deleted_at = now(), updated_at = now()
        FROM banksms.raw_messages r
        WHERE t.raw_message_id = r.id
          AND r.skeleton_hash = $1
          AND t.source = 'whatsapp'
          AND t.deleted_at IS NULL
        "#,
    )
    .bind(&hash)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(affected)
}

/// Promote an `ignored` message back into the pipeline: a human says this really
/// was a transaction. Removes any noise-skeleton entry so the promotion sticks.
pub async fn reclassify_as_transaction(pool: &sqlx::PgPool, raw_id: i64) -> AppResult<()> {
    let mut tx = pool.begin().await?;

    let hash: Option<Option<String>> =
        sqlx::query_scalar("SELECT skeleton_hash FROM banksms.raw_messages WHERE id = $1")
            .bind(raw_id)
            .fetch_optional(&mut *tx)
            .await?;

    if let Some(Some(hash)) = hash {
        sqlx::query("DELETE FROM banksms.noise_skeletons WHERE skeleton_hash = $1")
            .bind(&hash)
            .execute(&mut *tx)
            .await?;
    }

    sqlx::query("UPDATE banksms.raw_messages SET parse_status = 'pending' WHERE id = $1")
        .bind(raw_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(())
}
