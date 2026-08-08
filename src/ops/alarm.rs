//! The alarm condition.
//!
//! From the ops spec, stated precisely because getting it wrong makes the whole
//! alerting setup useless:
//!
//!   THE alarm is a `skeleton_hash` recurring N or more times with no matching
//!   template. That means a bank changed its SMS format and every message in
//!   that format is losing data.
//!
//!   A ONE-OFF unmatched message is almost always a human message that slipped
//!   past the triage gate, and must NOT page anyone.
//!
//! Recon turned up one honest counter-example: the CIB card-withdrawal format
//! appeared exactly once in seven months of history and was a real bank template.
//! So recurrence is a strong prior, not proof -- one-offs still land in the
//! review queue, they just do not raise an alarm.

use log::{error, info};

use crate::errors::AppResult;
use crate::ops::metrics::{self, ALARM_UNMATCHED_SKELETONS};

/// How many times a format must recur before it is treated as a bank change
/// rather than a stray human message.
pub const DEFAULT_ALARM_THRESHOLD: i64 = 3;

#[derive(Debug, Clone)]
pub struct UnmatchedFormat {
    pub skeleton_hash: String,
    pub occurrences: i64,
    pub example_body: String,
}

/// Find recurring formats with no template.
pub async fn check(pool: &sqlx::PgPool, threshold: i64) -> AppResult<Vec<UnmatchedFormat>> {
    let rows = sqlx::query_as::<_, (String, i64, String)>(
        r#"
        SELECT skeleton_hash,
               count(*) AS occurrences,
               (ARRAY_AGG(body ORDER BY wa_timestamp DESC))[1] AS example_body
        FROM banksms.raw_messages
        WHERE skeleton_hash IS NOT NULL
          AND parse_status IN ('partial', 'unmatched')
        GROUP BY skeleton_hash
        HAVING count(*) >= $1
        ORDER BY count(*) DESC
        "#,
    )
    .bind(threshold)
    .fetch_all(pool)
    .await?;

    let formats: Vec<UnmatchedFormat> = rows
        .into_iter()
        .map(|(skeleton_hash, occurrences, example_body)| UnmatchedFormat {
            skeleton_hash,
            occurrences,
            example_body,
        })
        .collect();

    ALARM_UNMATCHED_SKELETONS.store(formats.len() as u64, std::sync::atomic::Ordering::Relaxed);

    for f in &formats {
        // ERROR level on purpose: this is the one condition worth waking someone
        // for. The example body is included so the on-call engineer can write the
        // new template straight from the log line.
        error!(
            "ALARM: unhandled SMS format seen {} times (skeleton {}). A bank likely \
             changed its format. Example: {}",
            f.occurrences,
            f.skeleton_hash,
            f.example_body.chars().take(200).collect::<String>()
        );
    }

    if formats.is_empty() {
        info!("alarm check: no recurring unhandled formats");
    }

    Ok(formats)
}

/// Refresh the message-status counters from the database.
///
/// Process counters reset on restart; these are the authoritative totals.
pub async fn refresh_status_counters(pool: &sqlx::PgPool) -> AppResult<()> {
    let rows = sqlx::query_as::<_, (String, i64)>(
        "SELECT parse_status::text, count(*) FROM banksms.raw_messages GROUP BY 1",
    )
    .fetch_all(pool)
    .await?;

    use std::sync::atomic::Ordering;

    // Zero first. GROUP BY only returns statuses that still have rows, so a
    // status that has just dropped to zero would otherwise keep its previous
    // value forever -- leaving `partial` reading 4 immediately after a reparse
    // resolved all four, which is precisely when an operator is looking.
    for counter in [
        &metrics::MESSAGES_PARSED,
        &metrics::MESSAGES_PARTIAL,
        &metrics::MESSAGES_UNMATCHED,
        &metrics::MESSAGES_IGNORED,
    ] {
        counter.store(0, Ordering::Relaxed);
    }

    for (status, n) in rows {
        let counter = match status.as_str() {
            "parsed" => &metrics::MESSAGES_PARSED,
            "partial" => &metrics::MESSAGES_PARTIAL,
            "unmatched" => &metrics::MESSAGES_UNMATCHED,
            "ignored" => &metrics::MESSAGES_IGNORED,
            _ => continue,
        };
        counter.store(n as u64, Ordering::Relaxed);
    }

    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM banksms.raw_messages")
        .fetch_one(pool)
        .await?;
    metrics::MESSAGES_INGESTED.store(total as u64, Ordering::Relaxed);

    let txns: i64 =
        sqlx::query_scalar("SELECT count(*) FROM banksms.transactions WHERE deleted_at IS NULL")
            .fetch_one(pool)
            .await?;
    metrics::TRANSACTIONS_CREATED.store(txns as u64, Ordering::Relaxed);

    // The alarm gauge must be refreshed here too, not only in the poll loop.
    // Otherwise an API-only instance (or one whose poller is disabled) reports a
    // stale 0 while a bank format is actively breaking -- a monitoring gap that
    // looks exactly like "everything is fine".
    let unmatched_formats: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (
             SELECT skeleton_hash FROM banksms.raw_messages
             WHERE skeleton_hash IS NOT NULL
               AND parse_status IN ('partial', 'unmatched')
             GROUP BY skeleton_hash
             HAVING count(*) >= $1
         ) recurring",
    )
    .bind(DEFAULT_ALARM_THRESHOLD)
    .fetch_one(pool)
    .await?;
    metrics::ALARM_UNMATCHED_SKELETONS.store(unmatched_formats as u64, Ordering::Relaxed);

    Ok(())
}
