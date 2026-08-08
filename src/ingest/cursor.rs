//! The ingest cursor.
//!
//! Composite on purpose: `(last_wa_timestamp, last_wa_message_id)`. Timestamp
//! alone is unsafe because several messages routinely share the same second and
//! the API's ordering within a second is not guaranteed stable, so a
//! timestamp-only cursor can skip a message that arrives "at the same time" as
//! the one it stopped on.
//!
//! Reads and writes always target the singleton row (`id = 1`), which the schema
//! enforces with a CHECK constraint.

use chrono::{DateTime, Duration, Utc};
use sqlx::{Postgres, Transaction};

use crate::config::CONFIG;
use crate::errors::AppResult;

#[derive(Debug, Clone)]
pub struct Cursor {
    pub chat_jid: Option<String>,
    pub last_wa_timestamp: Option<DateTime<Utc>>,
    pub last_wa_message_id: Option<String>,
    pub last_poll_at: Option<DateTime<Utc>>,
    pub consecutive_errors: i32,
}

impl Cursor {
    /// Where the next poll should start reading from.
    ///
    /// Deliberately rewinds by `OVERLAP_WINDOW_SECS` rather than resuming exactly
    /// at the cursor: WhatsApp delivers backlogs in bursts once the phone
    /// reconnects, so messages older than the cursor keep arriving. The unique
    /// constraint on `wa_message_id` makes re-reading free, so the window is
    /// generous by default (5 minutes).
    ///
    /// `None` means "no cursor yet" -- read from the beginning of history.
    pub fn poll_from(&self) -> Option<DateTime<Utc>> {
        self.last_wa_timestamp
            .map(|ts| ts - Duration::seconds(CONFIG.overlap_window_secs))
    }
}

pub async fn load(pool: &sqlx::PgPool) -> AppResult<Cursor> {
    let row = sqlx::query!(
        r#"
        SELECT chat_jid, last_wa_timestamp, last_wa_message_id,
               last_poll_at, consecutive_errors
        FROM banksms.ingest_cursor
        WHERE id = 1
        "#
    )
    .fetch_one(pool)
    .await?;

    Ok(Cursor {
        chat_jid: row.chat_jid,
        last_wa_timestamp: row.last_wa_timestamp,
        last_wa_message_id: row.last_wa_message_id,
        last_poll_at: row.last_poll_at,
        consecutive_errors: row.consecutive_errors,
    })
}

/// Advance the cursor. MUST be called inside the same transaction as the batch
/// insert it corresponds to -- that is what makes a crash mid-batch replay
/// rather than skip.
///
/// Guarded with `GREATEST` so an out-of-order or late batch can never move the
/// cursor backwards.
pub async fn advance(
    tx: &mut Transaction<'_, Postgres>,
    chat_jid: &str,
    newest_timestamp: DateTime<Utc>,
    newest_message_id: &str,
) -> AppResult<()> {
    sqlx::query!(
        r#"
        UPDATE banksms.ingest_cursor
        SET chat_jid           = $1,
            last_wa_timestamp  = GREATEST(COALESCE(last_wa_timestamp, $2), $2),
            last_wa_message_id = CASE
                                     WHEN last_wa_timestamp IS NULL
                                       OR $2 >= last_wa_timestamp THEN $3
                                     ELSE last_wa_message_id
                                 END,
            last_poll_at       = now(),
            last_error         = NULL,
            consecutive_errors = 0,
            updated_at         = now()
        WHERE id = 1
        "#,
        chat_jid,
        newest_timestamp,
        newest_message_id
    )
    .execute(&mut **tx)
    .await?;

    Ok(())
}

/// Record a successful poll that found nothing new. Keeps `/admin/ingest-status`
/// honest about liveness: without this, a healthy but quiet poller looks stalled.
pub async fn touch_poll(pool: &sqlx::PgPool) -> AppResult<()> {
    sqlx::query!(
        r#"
        UPDATE banksms.ingest_cursor
        SET last_poll_at = now(), last_error = NULL,
            consecutive_errors = 0, updated_at = now()
        WHERE id = 1
        "#
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn record_error(pool: &sqlx::PgPool, message: &str) -> AppResult<()> {
    sqlx::query!(
        r#"
        UPDATE banksms.ingest_cursor
        SET last_error          = $1,
            last_error_at       = now(),
            consecutive_errors  = consecutive_errors + 1,
            updated_at          = now()
        WHERE id = 1
        "#,
        message
    )
    .execute(pool)
    .await?;
    Ok(())
}
