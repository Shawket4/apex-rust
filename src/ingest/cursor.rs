//! The ingest cursor.
//!
//! Composite on purpose: `(last_wa_timestamp, last_wa_message_id)`. Timestamp
//! alone is unsafe because several messages routinely share the same second and
//! the API's ordering within a second is not guaranteed stable.
//!
//! The singleton row is created on first use — a fresh schema seeds nothing.

use chrono::{DateTime, Duration, Utc};
use sqlx::Row;

use crate::config::CONFIG;
use crate::errors::AppResult;

#[derive(Debug, Clone, Default)]
pub struct Cursor {
    pub last_wa_timestamp: Option<DateTime<Utc>>,
    pub last_wa_message_id: Option<String>,
    pub last_poll_at: Option<DateTime<Utc>>,
    pub consecutive_errors: i32,
}

impl Cursor {
    /// Where the next poll should start reading from.
    ///
    /// Deliberately rewinds by `OVERLAP_WINDOW_SECS`: WhatsApp delivers
    /// backlogs in bursts once the phone reconnects, so messages older than
    /// the cursor keep arriving. The unique constraint on `wa_message_id`
    /// makes re-reading free.
    ///
    /// `None` means "no cursor yet" — read from the beginning of history.
    pub fn poll_from(&self) -> Option<DateTime<Utc>> {
        self.last_wa_timestamp
            .map(|ts| ts - Duration::seconds(CONFIG.overlap_window_secs))
    }
}

pub async fn load(pool: &sqlx::PgPool) -> AppResult<Cursor> {
    let row = sqlx::query(
        "SELECT last_wa_timestamp, last_wa_message_id, last_poll_at, consecutive_errors
         FROM banksms.ingest_cursor WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?;

    Ok(match row {
        Some(r) => Cursor {
            last_wa_timestamp: r.get("last_wa_timestamp"),
            last_wa_message_id: r.get("last_wa_message_id"),
            last_poll_at: r.get("last_poll_at"),
            consecutive_errors: r.get("consecutive_errors"),
        },
        None => Cursor::default(),
    })
}

/// Advance the cursor — called ONCE per poll cycle, after every page has
/// committed, never per page. The API returns newest-first while the poller
/// pages downward into older messages; advancing per page would push the
/// cursor to the newest message while older pages were still unfetched, and a
/// crash at that point would silently skip them forever. Advancing at the end
/// means a crash mid-cycle leaves the cursor untouched and the next cycle
/// re-reads the whole range — replay, never skip.
///
/// `GREATEST`-guarded so a late or out-of-order batch can never move it
/// backwards.
pub async fn advance(
    pool: &sqlx::PgPool,
    chat_jid: &str,
    newest_timestamp: DateTime<Utc>,
    newest_message_id: &str,
) -> AppResult<()> {
    sqlx::query(
        r#"
        INSERT INTO banksms.ingest_cursor
            (id, chat_jid, last_wa_timestamp, last_wa_message_id, last_poll_at, updated_at)
        VALUES (1, $1, $2, $3, now(), now())
        ON CONFLICT (id) DO UPDATE SET
            chat_jid           = EXCLUDED.chat_jid,
            last_wa_timestamp  = GREATEST(COALESCE(banksms.ingest_cursor.last_wa_timestamp, EXCLUDED.last_wa_timestamp), EXCLUDED.last_wa_timestamp),
            last_wa_message_id = CASE
                                     WHEN banksms.ingest_cursor.last_wa_timestamp IS NULL
                                       OR EXCLUDED.last_wa_timestamp >= banksms.ingest_cursor.last_wa_timestamp
                                     THEN EXCLUDED.last_wa_message_id
                                     ELSE banksms.ingest_cursor.last_wa_message_id
                                 END,
            last_poll_at       = now(),
            last_error         = NULL,
            consecutive_errors = 0,
            updated_at         = now()
        "#,
    )
    .bind(chat_jid)
    .bind(newest_timestamp)
    .bind(newest_message_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a successful poll that found nothing new — a healthy but quiet
/// poller must not look stalled.
pub async fn touch_poll(pool: &sqlx::PgPool, chat_jid: &str) -> AppResult<()> {
    sqlx::query(
        r#"
        INSERT INTO banksms.ingest_cursor (id, chat_jid, last_poll_at, updated_at)
        VALUES (1, $1, now(), now())
        ON CONFLICT (id) DO UPDATE SET
            last_poll_at = now(), last_error = NULL,
            consecutive_errors = 0, updated_at = now()
        "#,
    )
    .bind(chat_jid)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn record_error(pool: &sqlx::PgPool, chat_jid: &str, message: &str) -> AppResult<()> {
    sqlx::query(
        r#"
        INSERT INTO banksms.ingest_cursor (id, chat_jid, last_error, last_error_at, consecutive_errors, updated_at)
        VALUES (1, $1, $2, now(), 1, now())
        ON CONFLICT (id) DO UPDATE SET
            last_error          = EXCLUDED.last_error,
            last_error_at       = now(),
            consecutive_errors  = banksms.ingest_cursor.consecutive_errors + 1,
            updated_at          = now()
        "#,
    )
    .bind(chat_jid)
    .bind(message)
    .execute(pool)
    .await?;
    Ok(())
}
