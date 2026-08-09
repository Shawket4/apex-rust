//! Background ingestion poller.
//!
//! Runs as an independent tokio task, decoupled from the HTTP server: parsing
//! and serving never block ingestion, and ingestion never blocks a request.
//!
//! Invariants this module exists to protect:
//!   * A crash mid-batch REPLAYS, never skips. Batch insert and cursor advance
//!     happen in one transaction.
//!   * Two instances cannot double-poll. Each cycle holds a Postgres advisory
//!     lock.
//!   * Re-reading is free. `ON CONFLICT (wa_message_id) DO NOTHING` plus the
//!     overlap window means late-delivered messages get picked up without
//!     duplicating anything.
//!   * Nothing is dropped. Every fetched message is stored verbatim before any
//!     parsing is attempted.

use chrono::{DateTime, Utc};
use log::{debug, error, info, warn};
use std::time::Duration;

use crate::config::CONFIG;
use crate::errors::{AppError, AppResult};
use crate::ingest::cursor::{self, Cursor};
use crate::ingest::whatsapp_client::{WaMessage, WhatsAppClient, MAX_PAGE_LIMIT};
use crate::parser::worker;

/// Advisory-lock key for the poll cycle. Arbitrary but must stay stable: changing
/// it would let an old and a new build poll concurrently during a deploy.
const POLL_LOCK_KEY: i64 = 0x62_61_6E_6B_73_6D_73; // "banksms" in hex bytes

#[derive(Debug, Default)]
pub struct PollOutcome {
    pub fetched: usize,
    pub inserted: usize,
    pub pages: usize,
    /// False when another instance held the lock, so this cycle did nothing.
    pub ran: bool,
}

/// Insert a batch of raw messages, skipping ones already stored.
///
/// Returns the number of genuinely new rows. Bodies are stored verbatim --
/// normalization happens on a copy at parse time, so the original is always
/// recoverable.
async fn insert_batch(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    messages: &[WaMessage],
) -> AppResult<usize> {
    if messages.is_empty() {
        return Ok(0);
    }

    // UNNEST rather than a loop: one round-trip per batch instead of per message.
    let ids: Vec<String> = messages.iter().map(|m| m.id.clone()).collect();
    let chat_jids: Vec<String> = messages.iter().map(|m| m.chat_jid.clone()).collect();
    let senders: Vec<Option<String>> = messages.iter().map(|m| m.sender_jid.clone()).collect();
    let from_me: Vec<bool> = messages.iter().map(|m| m.is_from_me).collect();
    let timestamps: Vec<DateTime<Utc>> = messages.iter().map(|m| m.timestamp).collect();
    let bodies: Vec<String> = messages.iter().map(|m| m.content.clone()).collect();

    let inserted = sqlx::query!(
        r#"
        INSERT INTO banksms.raw_messages
            (wa_message_id, chat_jid, sender, is_from_me, wa_timestamp, body)
        SELECT * FROM UNNEST(
            $1::text[], $2::text[], $3::text[], $4::bool[], $5::timestamptz[], $6::text[]
        )
        ON CONFLICT (wa_message_id) DO NOTHING
        "#,
        &ids,
        &chat_jids,
        &senders as &[Option<String>],
        &from_me,
        &timestamps,
        &bodies
    )
    .execute(&mut **tx)
    .await?
    .rows_affected();

    Ok(inserted as usize)
}

/// One poll cycle: fetch everything at or after the cursor (minus the overlap
/// window), store it, advance the cursor.
///
/// Returns `ran: false` if another instance held the advisory lock.
pub async fn poll_once(pool: &sqlx::PgPool, client: &WhatsAppClient) -> AppResult<PollOutcome> {
    let chat_jid = CONFIG.target_chat_jid.clone();
    if chat_jid.is_empty() {
        return Err(AppError::Internal(
            "TARGET_CHAT_JID is not configured".to_string(),
        ));
    }

    // The advisory lock is session-scoped, so it must be taken and released on
    // one connection that stays checked out for the whole cycle.
    let mut conn = pool.acquire().await?;

    let got_lock: bool = sqlx::query_scalar!("SELECT pg_try_advisory_lock($1)", POLL_LOCK_KEY)
        .fetch_one(&mut *conn)
        .await?
        .unwrap_or(false);

    if !got_lock {
        debug!("poll skipped: another instance holds the ingest lock");
        crate::ops::metrics::incr(&crate::ops::metrics::POLL_SKIPPED_LOCKED, 1);
        return Ok(PollOutcome {
            ran: false,
            ..Default::default()
        });
    }

    // Always release, including on the error paths below.
    let result = poll_locked(pool, client, &chat_jid).await;

    if let Err(e) = sqlx::query_scalar!("SELECT pg_advisory_unlock($1)", POLL_LOCK_KEY)
        .fetch_one(&mut *conn)
        .await
    {
        // Not fatal: the lock dies with the session anyway.
        warn!("failed to release ingest advisory lock: {e}");
    }

    result
}

async fn poll_locked(
    pool: &sqlx::PgPool,
    client: &WhatsAppClient,
    chat_jid: &str,
) -> AppResult<PollOutcome> {
    let cur: Cursor = cursor::load(pool).await?;
    let start_time = cur.poll_from();

    let mut outcome = PollOutcome {
        ran: true,
        ..Default::default()
    };
    let mut offset: u32 = 0;

    // The newest message seen anywhere in this cycle. The cursor is advanced to
    // this ONCE, after every page has been committed -- never per page.
    //
    // This matters because the API returns newest-first and we page downwards
    // into older messages. Advancing per page would push the cursor to the newest
    // message while pages of OLDER messages were still unfetched; a crash at that
    // point would leave the cursor in the future and those older messages would
    // never be read again. That is a silent skip, which is the one failure mode
    // this poller must not have.
    //
    // Advancing at the end instead means a crash mid-cycle leaves the cursor
    // untouched, so the next cycle re-reads the whole range. Re-reading is free
    // (ON CONFLICT DO NOTHING on wa_message_id), so replay is strictly preferable
    // to any risk of skipping.
    let mut newest_seen: Option<(DateTime<Utc>, String)> = None;

    loop {
        let batch = client
            .list_messages(chat_jid, start_time, MAX_PAGE_LIMIT, offset)
            .await?;

        if batch.is_empty() {
            break;
        }

        outcome.fetched += batch.len();
        outcome.pages += 1;

        // Defensive: the endpoint is already chat-scoped, but never let a message
        // from another chat into the store on the strength of that assumption.
        let batch: Vec<WaMessage> = batch
            .into_iter()
            .filter(|m| m.chat_jid == chat_jid)
            .collect();

        if let Some(page_newest) = batch.iter().max_by_key(|m| (m.timestamp, m.id.clone())) {
            let candidate = (page_newest.timestamp, page_newest.id.clone());
            newest_seen = match newest_seen {
                Some(current) if current >= candidate => Some(current),
                _ => Some(candidate),
            };
        }

        let mut tx = pool.begin().await?;
        let inserted = insert_batch(&mut tx, &batch).await?;
        tx.commit().await?;

        outcome.inserted += inserted;

        // `pagination.total` from the API is the chat's unfiltered total, so it
        // cannot tell us whether more pages match `start_time`. Page until short.
        if batch.len() < MAX_PAGE_LIMIT as usize {
            break;
        }
        offset += MAX_PAGE_LIMIT;

        // Safety valve. A cursor that never advances plus an API that keeps
        // returning full pages would otherwise spin forever.
        if outcome.pages >= 200 {
            warn!(
                "poll stopped at {} pages ({} fetched); will continue next cycle",
                outcome.pages, outcome.fetched
            );
            break;
        }
    }

    // Every page is now durably stored, so it is safe to declare the range read.
    match newest_seen {
        Some((ts, id)) => {
            let mut tx = pool.begin().await?;
            cursor::advance(&mut tx, chat_jid, ts, &id).await?;
            tx.commit().await?;
        }
        // Nothing fetched at all: still record the poll so ingest-status can tell
        // "healthy but quiet" from "stalled".
        None => cursor::touch_poll(pool).await?,
    }

    Ok(outcome)
}

/// Long-running poll loop. Spawn once at startup.
///
/// Backoff is exponential with jitter and capped. The jitter matters: without it
/// two instances that error at the same moment retry in lockstep forever.
pub async fn run(pool: sqlx::PgPool, client: WhatsAppClient) {
    if CONFIG.target_chat_jid.is_empty() {
        warn!("ingest disabled: TARGET_CHAT_JID is not set");
        return;
    }

    info!(
        "ingest poller started (chat={}, interval={}s, overlap={}s)",
        CONFIG.target_chat_jid, CONFIG.poll_interval_secs, CONFIG.overlap_window_secs
    );

    // Drain anything already sitting at `pending` before the first poll. Without
    // this, messages ingested by `backfill` (or by a build that had no parse
    // step) would stay unparsed forever -- the poller only ever parsed what it
    // had just fetched itself.
    drain_pending(&pool).await;

    let mut consecutive_failures: u32 = 0;

    loop {
        match poll_once(&pool, &client).await {
            Ok(outcome) => {
                consecutive_failures = 0;
                crate::ops::metrics::incr(&crate::ops::metrics::POLL_CYCLES, 1);
                crate::ops::metrics::incr(
                    &crate::ops::metrics::MESSAGES_INGESTED,
                    outcome.inserted as u64,
                );
                if outcome.inserted > 0 {
                    info!(
                        "ingest: {} new of {} fetched across {} page(s)",
                        outcome.inserted, outcome.fetched, outcome.pages
                    );

                    // Parse what was just stored. Deliberately after the ingest
                    // transaction committed: a parser failure must never be able
                    // to roll back or delay an ingest.
                    if let Err(e) =
                        crate::parser::worker::run(&pool, crate::parser::worker::Scope::Pending, 50_000)
                            .await
                    {
                        error!("parse run failed (messages are stored and will retry): {e}");
                        crate::ops::metrics::incr(&crate::ops::metrics::PARSE_ERRORS, 1);
                    }

                    // A bank changing its SMS format is the one thing worth
                    // paging for, so check after every batch that produced work.
                    if let Err(e) = crate::ops::alarm::check(
                        &pool,
                        crate::ops::alarm::DEFAULT_ALARM_THRESHOLD,
                    )
                    .await
                    {
                        warn!("alarm check failed: {e}");
                    }
                } else if outcome.ran {
                    debug!("ingest: nothing new ({} fetched)", outcome.fetched);
                }
                tokio::time::sleep(Duration::from_secs(CONFIG.poll_interval_secs)).await;
            }
            Err(e) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                crate::ops::metrics::incr(&crate::ops::metrics::POLL_ERRORS, 1);
                error!("ingest poll failed (attempt {consecutive_failures}): {e}");

                if let Err(log_err) = cursor::record_error(&pool, &e.to_string()).await {
                    error!("could not record ingest error: {log_err}");
                }

                let delay = backoff_delay(consecutive_failures, CONFIG.poll_interval_secs);
                warn!("backing off {delay:?} before next poll");
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// Parse every `pending` message, logging rather than propagating failures.
///
/// Parsing is best-effort by design: a bad template or a panic-adjacent edge
/// case should degrade to "these rows stay pending and get retried next cycle",
/// never to "ingestion stops".
async fn drain_pending(pool: &sqlx::PgPool) {
    match worker::run(pool, worker::Scope::Pending, 100_000).await {
        Ok(run) if run.examined > 0 => info!(
            "parse: examined {}, parsed {}, partial {}, unmatched {}, ignored {}, {} transaction(s) created",
            run.examined, run.parsed, run.partial, run.unmatched, run.ignored,
            run.transactions_created
        ),
        Ok(_) => {}
        Err(e) => error!("parse run failed: {e}"),
    }
}

/// Exponential backoff with full jitter, capped at `POLL_BACKOFF_MAX_SECS`.
fn backoff_delay(attempt: u32, base_secs: u64) -> Duration {
    let base = base_secs.max(1);
    let exp = base.saturating_mul(1u64 << attempt.min(10).saturating_sub(1).min(16));
    let capped = exp.min(CONFIG.poll_backoff_max_secs.max(base));

    // Full jitter: sleep a uniform random amount in [base, capped]. Derived from
    // the clock rather than pulling in a RNG dependency; the quality needed here
    // is "don't retry in lockstep", not cryptographic.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);

    let span = capped.saturating_sub(base);
    let jitter = if span == 0 { 0 } else { nanos % (span + 1) };

    Duration::from_secs(base + jitter)
}

/// One-shot backfill: walk history from the oldest end forwards, feeding the same
/// insert path the poller uses, so there is no second code path that could store
/// messages differently.
///
/// Deliberately does NOT touch the cursor: the cursor tracks the live tail, and
/// moving it based on a historical walk would make the poller skip the present.
pub async fn backfill(pool: &sqlx::PgPool, client: &WhatsAppClient) -> AppResult<usize> {
    let chat_jid = CONFIG.target_chat_jid.clone();
    if chat_jid.is_empty() {
        return Err(AppError::Internal(
            "TARGET_CHAT_JID is not configured".to_string(),
        ));
    }

    info!("backfill starting for chat {chat_jid}");

    let mut offset: u32 = 0;
    let mut total_inserted = 0usize;
    let mut total_fetched = 0usize;

    loop {
        let batch = client
            .list_messages(&chat_jid, None, MAX_PAGE_LIMIT, offset)
            .await?;

        if batch.is_empty() {
            break;
        }

        total_fetched += batch.len();
        let batch_len = batch.len();

        let batch: Vec<WaMessage> = batch
            .into_iter()
            .filter(|m| m.chat_jid == chat_jid)
            .collect();

        let mut tx = pool.begin().await?;
        let inserted = insert_batch(&mut tx, &batch).await?;
        tx.commit().await?;

        total_inserted += inserted;
        info!(
            "backfill: offset {offset}, {inserted} new of {batch_len} ({total_inserted} total)"
        );

        if batch_len < MAX_PAGE_LIMIT as usize {
            break;
        }
        offset += MAX_PAGE_LIMIT;
    }

    info!("backfill complete: {total_inserted} new of {total_fetched} fetched");

    // Parse in the same run. Otherwise a fresh backfill leaves every message at
    // `pending` and the operator sees no transactions until the poller happens
    // to fetch something new.
    drain_pending(pool).await;

    Ok(total_inserted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_stays_capped() {
        // Never below the base interval, never above the configured ceiling.
        for attempt in 1..=20 {
            let d = backoff_delay(attempt, 60).as_secs();
            assert!(d >= 60, "attempt {attempt} gave {d}s, below base");
            assert!(
                d <= CONFIG.poll_backoff_max_secs.max(60),
                "attempt {attempt} gave {d}s, above cap"
            );
        }
    }

    #[test]
    fn backoff_first_attempt_is_base() {
        // attempt 1 => shift 0 => exactly the base interval, plus jitter of 0.
        assert_eq!(backoff_delay(1, 30).as_secs(), 30);
    }
}
